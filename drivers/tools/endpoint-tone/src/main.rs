//! 只放音、不采集：把一段正弦波播到**显式选定**的 Windows 渲染端点。
//!
//! 存在理由（research/vad/driver-smoke.md 阶段 4 冒烟需要）：
//! 仓库里的 `miplay-audio --bin loopback` 在 `crates/miplay-audio/src/bin/loopback.rs:119`
//! 会先 `AudioCapture::new(cfg)?` 建立**采集**流，再开始播放；Scream 派生驱动只暴露
//! render pin（无 capture pin），因此那个 bin 无法用来往该端点放音。
//!
//! 安全性：本工具**拒绝默认设备**——必须显式给 `--name` 或 `--id`，避免在验证过程中
//! 往用户当前默认播放设备灌声音（Lead 的约束：不改默认播放设备）。
//!
//! 用法：
//!   cargo run --release --manifest-path drivers/tools/endpoint-tone/Cargo.toml -- `
//!       --name 妙播 --freq 1000 --seconds 5 --amp 0.4

use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use miplay_audio::DeviceSelector;

const USAGE: &str = "\
endpoint-tone —— 往显式选定的渲染端点播一段正弦（只放音，不采集）

用法:
  endpoint-tone (--name <子串> | --id <端点ID>) [选项]

选项:
  --name <子串>     按设备名子串选择端点（推荐，如 --name 妙播）
  --id <端点ID>     按端点 ID 选择（与 --name 二选一）
  --freq <Hz>       正弦频率，默认 1000
  --seconds <秒>    时长，默认 5
  --amp <0..1>      幅度，默认 0.4
  --delay-ms <毫秒> 开始播放前的等待，默认 2000（留时间先启动 UDP 接收端）
  -h, --help        显示本帮助

退出码: 0 成功；2 参数错误；1 播放失败。
";

struct Args {
    selector: DeviceSelector,
    freq: f64,
    seconds: f64,
    amp: f32,
    delay_ms: u64,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn parse_args() -> Result<Args, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        std::process::exit(0);
    }
    let mut name: Option<String> = None;
    let mut id: Option<String> = None;
    let mut freq = 1000.0_f64;
    let mut seconds = 5.0_f64;
    let mut amp = 0.4_f32;
    let mut delay_ms = 2000_u64;

    let mut i = 0;
    while i < argv.len() {
        let flag = argv[i].as_str();
        let value = || -> Result<String, String> {
            argv.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{flag} 缺少参数值"))
        };
        match flag {
            "--name" => {
                let v = value()?;
                name = Some(match name {
                    Some(prev) => format!("{prev} {v}"),
                    None => v,
                });
                i += 2;
            }
            "--id" => {
                id = Some(value()?);
                i += 2;
            }
            "--freq" => {
                freq = value()?
                    .parse()
                    .map_err(|e| format!("--freq 不是数字: {e}"))?;
                i += 2;
            }
            "--seconds" => {
                seconds = value()?
                    .parse()
                    .map_err(|e| format!("--seconds 不是数字: {e}"))?;
                i += 2;
            }
            "--amp" => {
                amp = value()?
                    .parse()
                    .map_err(|e| format!("--amp 不是数字: {e}"))?;
                i += 2;
            }
            "--delay-ms" => {
                delay_ms = value()?
                    .parse()
                    .map_err(|e| format!("--delay-ms 不是数字: {e}"))?;
                i += 2;
            }
            other => return Err(format!("未知参数: {other}")),
        }
    }

    let selector = match (name, id) {
        (Some(n), None) => DeviceSelector::name_contains(n),
        (None, Some(i)) => DeviceSelector::id(i),
        (Some(_), Some(_)) => return Err("--name 与 --id 只能给一个".into()),
        (None, None) => {
            return Err("必须显式指定 --name 或 --id（本工具拒绝默认设备，避免误播到用户扬声器）".into())
        }
    };

    if freq <= 0.0 || freq > 20_000.0 {
        return Err(format!("--freq 超出合理范围: {freq}"));
    }
    if seconds <= 0.0 || seconds > 600.0 {
        return Err(format!("--seconds 超出合理范围: {seconds}"));
    }
    if !(0.0..=1.0).contains(&amp) {
        return Err(format!("--amp 需要在 0..1: {amp}"));
    }

    Ok(Args {
        selector,
        freq,
        seconds,
        amp,
        delay_ms,
    })
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("参数错误: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    println!("[{}] 选择器: {}", now_ms(), args.selector.describe());
    println!(
        "[{}] 正弦: {:.1} Hz / {:.2} s / amp {:.2}（格式 44100/2ch/16，由播放层重采样到端点格式）",
        now_ms(),
        args.freq,
        args.seconds,
        args.amp
    );
    println!(
        "[{}] {:.1} 秒后开始播放——现在去启动 UDP 接收端（scream-udp-capture.py）",
        now_ms(),
        args.delay_ms as f64 / 1000.0
    );
    std::thread::sleep(Duration::from_millis(args.delay_ms));

    let started = now_ms();
    println!("[{started}] 开始播放");
    match miplay_audio::play_sine(&args.selector, args.freq, args.seconds, args.amp) {
        Ok(info) => {
            println!("[{}] 播放完成: {}", now_ms(), info.describe());
            // 让端点尾部数据有机会被驱动送出去，再释放设备
            std::thread::sleep(Duration::from_millis(500));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[{}] 播放失败: {e}", now_ms());
            ExitCode::FAILURE
        }
    }
}
