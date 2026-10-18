//! `balance_probe` —— 左右音箱音量平衡探针（task-22）。
//!
//! ```text
//! cargo run -p stereo-audio --example balance_probe -- --list
//! cargo run -p stereo-audio --example balance_probe -- \
//!     --mic "麦克风" --left "音箱 A" --right "音箱 B"
//! cargo run -p stereo-audio --example balance_probe -- \
//!     --mic "麦克风" --left "音箱 A" --right "板载输出" --apply
//! ```
//!
//! 做两件事：
//!
//! 1. **只**往 `--left` 端点放一段 1 kHz 测试音，用 `--mic` 独占模式录下来量电平；
//!    再**只**往 `--right` 放同一段，量电平。两路分开放、分开测，避免串音把两路
//!    混淆。
//! 2. 用 [`stereo_audio::sync::measure_balance`] 把两路电平差换算成「建议把哪个端点
//!    的音量调到百分之几」：较响的一路为基准不动，抬较安静的一路；抬到 100% 仍不够
//!    就改把较响的一路压下来，使两者相等。
//!
//! * `--apply`：把建议值用 `set_endpoint_volume` 真正写回两个端点（写后读回校验）。
//!   不加 `--apply` 时**只打印建议、不改任何设置**。
//! * `--probe-ms <n>`：测试音时长，默认 600 ms（夹到 200~2000）。
//! * `--amplitude <0..1>`：测试音幅度，默认 0.5（比标定用的 0.9 小，避免太吵）。
//! * `--list`：列出本机采集端点（麦克风候选）与 render 端点，方便抄名字。

use std::error::Error;

use stereo_audio::sync::{list_microphones, measure_balance, MicCandidate, SyncConfig};
use stereo_audio::volume::{get_endpoint_volume, set_endpoint_volume};
use stereo_audio::{list_endpoints, DataFlow};

/// `--key value` 取值；`key` 也可以写成 `--key=value`。
fn arg_value(args: &[String], key: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == key {
            return it.next().cloned();
        }
        if let Some(rest) = a.strip_prefix(&format!("{key}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

/// 命令行里有没有这个开关。
fn has_flag(args: &[String], key: &str) -> bool {
    args.iter().any(|a| a == key)
}

/// 按名字/ID 子串挑一个采集端点。
fn pick_mic(spec: &str) -> Result<MicCandidate, Box<dyn Error>> {
    let all = list_microphones();
    if all.is_empty() {
        return Err("本机没有任何采集端点".into());
    }
    let lower = spec.to_ascii_lowercase();
    let hit = all.iter().find(|m| {
        m.name().to_ascii_lowercase().contains(&lower)
            || m.id().to_ascii_lowercase().contains(&lower)
    });
    match hit {
        Some(m) => {
            println!("麦克风：{}", m.describe());
            Ok(m.clone())
        }
        None => {
            let names: Vec<String> = all.iter().map(|m| m.name().to_string()).collect();
            Err(format!("找不到麦克风「{spec}」；可用：{}", names.join(" / ")).into())
        }
    }
}

/// 打印两路端点当前的音量（`EndpointVolume::describe()`）。
fn print_current(tag: &str, spec: &str) -> Result<(), Box<dyn Error>> {
    match get_endpoint_volume(spec) {
        Ok(v) => println!("  {tag} {spec} -> {}", v.describe()),
        Err(e) => println!("  {tag} {spec} -> 读不到音量：{e}"),
    }
    Ok(())
}

/// `--list`：列出采集与 render 端点。
fn list_mode() -> Result<(), Box<dyn Error>> {
    println!("=== 采集端点（麦克风候选）===");
    for m in list_microphones() {
        println!("  {}", m.describe());
    }
    println!("=== render 端点 ===");
    for e in list_endpoints()? {
        if e.flow == DataFlow::Render {
            println!("  {} [{}]", e.name, e.id);
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || has_flag(&args, "--help") || has_flag(&args, "-h") {
        println!(
            "用法：\n  balance_probe --list\n  balance_probe --mic <子串> --left <端点> --right <端点> [--apply] \
             [--probe-ms <n>] [--amplitude <0..1>]\n\n\
             端点参数支持名字子串或 ID（{{0.0.0.00000000}}.{{...}}）。"
        );
        return Ok(());
    }
    if has_flag(&args, "--list") {
        return list_mode();
    }

    let mic_spec = arg_value(&args, "--mic").ok_or("必须给 --mic <子串>")?;
    let left = arg_value(&args, "--left").ok_or("必须给 --left <端点>")?;
    let right = arg_value(&args, "--right").ok_or("必须给 --right <端点>")?;
    let apply = has_flag(&args, "--apply");

    let mut cfg = SyncConfig::default();
    if let Some(v) = arg_value(&args, "--probe-ms") {
        cfg.probe_ms = v
            .parse::<u32>()
            .map_err(|_| format!("--probe-ms 非法：{v}"))?;
    }
    if let Some(v) = arg_value(&args, "--amplitude") {
        cfg.amplitude = v
            .parse::<f32>()
            .map_err(|_| format!("--amplitude 非法：{v}"))?;
    }
    cfg.probe_ms = cfg.probe_ms.clamp(200, 2_000);
    cfg.amplitude = cfg.amplitude.clamp(0.05, 1.0);

    println!("=== 左右音量平衡测量 ===");
    println!(
        "  配置：测试音 {:.0} Hz，{} ms，幅度 {:.2}；录音窗口 {} ms（前置 {} + 测试 {} + 尾部 {}）",
        stereo_audio::sync::BALANCE_TONE_HZ,
        cfg.probe_ms,
        cfg.amplitude,
        cfg.pre_roll_ms + cfg.probe_ms + cfg.tail_ms,
        cfg.pre_roll_ms,
        cfg.probe_ms,
        cfg.tail_ms
    );
    let mic = pick_mic(&mic_spec)?;
    print_current("调整前", &left)?;
    print_current("调整前", &right)?;

    let started = std::time::Instant::now();
    let report = measure_balance(&mic, &left, &right, &cfg)?;
    println!(
        "\n--- 结果（耗时 {:.1} s）---",
        started.elapsed().as_secs_f64()
    );
    println!("  {}", report.describe());
    println!(
        "  左 {:.2} dB / 右 {:.2} dB / delta {:+.2} dB / 置信度 {:.2}",
        report.left_db, report.right_db, report.delta_db, report.confidence
    );
    if report.balanced() {
        println!(
            "  两路相差 {:.2} dB，在 ±{:.1} dB 死区内 → 建议维持现状。",
            report.delta_db,
            stereo_audio::sync::BALANCE_DEADBAND_DB
        );
    } else {
        let louder = if report.delta_db > 0.0 { "左" } else { "右" };
        println!(
            "  {louder}路更响 {:+.2} dB → 建议 左 {}% / 右 {}%。",
            report.delta_db.abs(),
            report.suggested_left_percent,
            report.suggested_right_percent
        );
    }
    if report.confidence < 0.5 {
        println!(
            "  ⚠ 置信度只有 {:.2}：测试音信噪比偏低（房间太吵 / 麦克风太远 / 音量太小），\
             这次建议别直接照抄。",
            report.confidence
        );
    }

    if apply {
        println!("\n--- 写回 ---");
        let l = set_endpoint_volume(&left, report.suggested_left_percent as f32 / 100.0)?;
        println!("  左 {left} -> {}", l.describe());
        let r = set_endpoint_volume(&right, report.suggested_right_percent as f32 / 100.0)?;
        println!("  右 {right} -> {}", r.describe());
        println!("  已写回。建议**再跑一次**确认 delta 落进死区（端点音量曲线不是线性的，一次不一定到位）。");
    } else {
        println!("\n  （只打印建议，未改动任何设置；要写回请加 --apply）");
    }
    Ok(())
}
