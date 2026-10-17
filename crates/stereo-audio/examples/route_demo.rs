//! 多设备声道路由真机自检：把「左 / 右 / 双声道」测试音送到虚拟声卡，由
//! [`stereo_audio::RouteEngineHandle`] 回环采下来后按路由分别推给各台音箱。
//!
//! 用法（先把要用的输出设备都连到本机）：
//! ```powershell
//! cargo run -p stereo-audio --example route_demo -- --left "Xiaomi Sound-1695" --right "Xiaomi Sound-5320" [--both "USB 耳机"] [--source "立体声链"]
//! ```
//!
//! 内置测试音每段 3 秒（**直接往采集源端点播放**，这样回环采集才采得到）：
//! * 0–3 s：只有左声道 440 Hz → `--left` 设备应只听到它，`--right` 设备静音；
//! * 3–6 s：只有右声道 880 Hz → `--right` 设备应只听到它，`--left` 设备静音；
//! * 6–9 s：左 440 Hz + 右 880 Hz → `--left` 只听到 440，`--right` 只听到 880，
//!   `--both` 听到两者混合（`(L+R)/2`）。
//!
//! 如果不想/不能自动播测试音，用 `--manual` 只启动路由引擎等外部声音
//! （此时请手动往 `--source` 端点播放音乐，然后观察每秒打印的各路电平/欠载数据）。
//!
//! 注意：`--source` 必须是 **render（播放）端点**（回环采集的就是它的输出）；蓝牙音箱
//! 做路由目标时要先连上本机并在 Windows 里出现为活动播放端点。结束时引擎自动停止，
//! 不会改系统默认输出，也不会留下后台线程。

use std::thread;
use std::time::{Duration, Instant};

use stereo_audio::{
    play_pcm_s16, DeviceSelector, PcmFormat, RouteChannel, RouteConfig, RouteEngineConfig,
    RouteEngineHandle,
};

const RATE: u32 = 44_100;
/// 每段测试音时长（秒）。
const SEGMENT_SECONDS: f64 = 3.0;

fn usage() -> &'static str {
    "用法:
  cargo run -p stereo-audio --example route_demo -- \\
      --left <左声道设备名子串> --right <右声道设备名子串> \\
      [--both <双声道设备名子串>] [--source <采集源设备名子串>] \\
      [--tone-to <测试音播放目标> | --manual] [--wait <秒>]

参数:
  --left <子串>     RouteChannel::Left 的路由（只送采集左声道）
  --right <子串>    RouteChannel::Right 的路由（只送采集右声道）
  --both <子串>     可选：RouteChannel::Both 的路由（送 (L+R)/2 混音）
  --source <子串>   回环采集的 render 端点，默认 \"立体声链\"
  --tone-to <子串>  内置测试音播到哪个端点（默认 = --source；传 none 关闭自动播放）
  --manual          不自动播测试音，只启动引擎等外部声音；跑 --wait 秒后自动停止
  --wait <秒>       --manual 模式下等待多久（默认 15）
  -h, --help        打印本帮助

说明:
  端点名按友好名子串匹配（大小写不敏感），例如 \"Xiaomi Sound-1695\"。
  同一端点不能出现在两条启用路由里（会抢设备，validate() 会拒绝）。"
}

/// 生成 9 秒测试音（44.1k/2ch/s16le 交错）：左 → 右 → 双声道各 3 秒。
fn build_test_tone() -> Vec<u8> {
    let mut pcm = Vec::new();
    pcm.extend(tone_pcm(SEGMENT_SECONDS, |t| (sine(440.0, t), 0.0)));
    pcm.extend(tone_pcm(SEGMENT_SECONDS, |t| (0.0, sine(880.0, t))));
    pcm.extend(tone_pcm(SEGMENT_SECONDS, |t| {
        (sine(440.0, t), sine(880.0, t))
    }));
    pcm
}

fn tone_pcm(seconds: f64, phase: fn(f64) -> (f32, f32)) -> Vec<u8> {
    let frames = (seconds * RATE as f64) as usize;
    let mut out = Vec::with_capacity(frames * 4);
    for index in 0..frames {
        let t = index as f64 / RATE as f64;
        let (left, right) = phase(t);
        let left = (left.clamp(-1.0, 1.0) * 0.45 * i16::MAX as f32) as i16;
        let right = (right.clamp(-1.0, 1.0) * 0.45 * i16::MAX as f32) as i16;
        out.extend_from_slice(&left.to_le_bytes());
        out.extend_from_slice(&right.to_le_bytes());
    }
    out
}

fn sine(hz: f64, t: f64) -> f32 {
    (2.0 * std::f64::consts::PI * hz * t).sin() as f32
}

fn value_for(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} 后面缺参数"))
}

struct Options {
    left: Option<String>,
    right: Option<String>,
    both: Option<String>,
    source: String,
    tone_target: Option<String>,
    wait_seconds: f64,
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut left = None;
    let mut right = None;
    let mut both = None;
    let mut source = None;
    let mut tone_target = None;
    let mut tone_disabled = false;
    let mut manual = false;
    let mut wait_seconds = 15.0f64;

    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        match flag {
            "--left" => left = Some(value_for(args, &mut index, flag)?),
            "--right" => right = Some(value_for(args, &mut index, flag)?),
            "--both" => both = Some(value_for(args, &mut index, flag)?),
            "--source" => source = Some(value_for(args, &mut index, flag)?),
            "--tone-to" => {
                let value = value_for(args, &mut index, flag)?;
                if value.eq_ignore_ascii_case("none") {
                    tone_disabled = true;
                } else {
                    tone_target = Some(value);
                }
            }
            "--manual" => manual = true,
            "--wait" => {
                let value = value_for(args, &mut index, flag)?;
                wait_seconds = value
                    .parse::<f64>()
                    .map_err(|e| format!("--wait 需要数字（秒）：{e}"))?;
            }
            "-h" | "--help" => return Err(String::new()), // 特殊：调用方打印 usage 后正常退出
            other => return Err(format!("未知参数 {other}")),
        }
        index += 1;
    }

    let source = source.unwrap_or_else(|| "立体声链".to_owned());
    let tone_target = if manual || tone_disabled {
        None
    } else {
        Some(tone_target.unwrap_or_else(|| source.clone()))
    };
    Ok(Options {
        left,
        right,
        both,
        source,
        tone_target,
        wait_seconds,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse_args(&args) {
        Ok(options) => options,
        Err(message) => {
            if message.is_empty() {
                // --help
                println!("{}", usage());
                return Ok(());
            }
            eprintln!("{message}\n\n{}", usage());
            std::process::exit(2);
        }
    };
    let (Some(left), Some(right)) = (options.left.clone(), options.right.clone()) else {
        eprintln!("缺少 --left / --right 参数。\n\n{}", usage());
        std::process::exit(2);
    };

    // 组装路由：left / right 必有，--both 可选。
    let mut routes = vec![
        RouteConfig::new(left.clone(), RouteChannel::Left),
        RouteConfig::new(right.clone(), RouteChannel::Right),
    ];
    if let Some(both) = options.both.clone() {
        routes.push(RouteConfig::new(both.clone(), RouteChannel::Both));
    }
    let config = RouteEngineConfig {
        source_endpoint: options.source.clone(),
        routes,
        capture_gain: 1.0,
        render_buffer_ms: 40,
        queue_ms: 40,
    };
    if let Err(e) = config.validate() {
        eprintln!("路由配置不合法：{e}\n\n{}", usage());
        std::process::exit(2);
    }

    println!("== 多设备声道路由自检 ==");
    println!("配置: {}", config.describe());
    println!(
        "软件侧缓冲估计: {} ms（不含蓝牙/USB 链路本身）",
        config.software_latency_ms()
    );

    let mut engine = RouteEngineHandle::start(config)?;
    println!("路由引擎已启动，稍等 300 ms 让各条输出链路起流…");
    thread::sleep(Duration::from_millis(300));

    let mut player = None;
    let deadline = match options.tone_target.clone() {
        Some(target) => {
            let pcm = build_test_tone();
            println!(
                "测试音时间表：0–3s 左 440Hz | 3–6s 右 880Hz | 6–9s 左 440Hz + 右 880Hz"
            );
            println!(
                "正在把测试音播到采集源端点 \"{target}\"（自动播放；听不到就检查 --source 是不是它的 render 端点）…"
            );
            let play_target = target.clone();
            player = Some(thread::spawn(move || {
                play_pcm_s16(
                    &DeviceSelector::name_contains(play_target),
                    PcmFormat::new(RATE, 2, 16),
                    &pcm,
                )
            }));
            Instant::now() + Duration::from_secs_f64(SEGMENT_SECONDS * 3.0 + 2.0)
        }
        None => {
            println!(
                "未自动播放测试音：请手动往采集源端点 \"{}\" 播放声音（例如系统播放器/浏览器）。",
                options.source
            );
            println!(
                "各路线路的电平/欠载/队列会每秒打印一次；{:.0} 秒后自动停止（--wait 可调）。",
                options.wait_seconds
            );
            Instant::now() + Duration::from_secs_f64(options.wait_seconds.max(1.0))
        }
    };

    let started = Instant::now();
    while Instant::now() < deadline {
        thread::sleep(Duration::from_secs(1));
        let stats = engine.stats();
        println!(
            "[{:4.1}s] {}",
            started.elapsed().as_secs_f64(),
            stats.describe()
        );
        if let Some(err) = &stats.source_error {
            eprintln!("采集故障：{err}");
            break;
        }
        if !stats.running {
            eprintln!("路由引擎已停止");
            break;
        }
    }

    match player {
        Some(handle) => match handle.join() {
            Ok(Ok(info)) => println!("测试音播放完成: {}", info.describe()),
            Ok(Err(e)) => println!(
                "测试音播放失败：{e}\n（可改用 --manual 手动往采集源播放声音，再看每秒打印的各路数据。）"
            ),
            Err(_) => println!("测试音播放线程 panic"),
        },
        None => {}
    }

    println!("最后快照: {}", engine.stats().describe());
    engine.stop();
    println!("== 自检结束 ==");
    Ok(())
}
