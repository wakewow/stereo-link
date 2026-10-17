//! 按设备真实能力探测可用输出格式（`IAudioClient::IsFormatSupported`，独占模式）。
//!
//! 用法：
//!
//! ```text
//! cargo run -q -p stereo-audio --example format_probe -- --endpoint "Xiaomi Sound-1695"
//! cargo run -q -p stereo-audio --example format_probe -- --endpoint default
//! cargo run -q -p stereo-audio --example format_probe -- --all
//! ```
//!
//! 每个端点打印：混音格式、当前设备格式、独占支持列表、共享可用列表，以及
//! 「当前设备格式是否在支持列表内」——最后一项能查出 GUI 之前把设备设成了
//! 它其实放不了的格式。
//!
//! 参数：
//! * `--endpoint <id|友好名子串|default>`：只探测一个端点；
//! * `--all`：遍历本机所有活动 render 端点；
//! * `--no-shared`：跳过共享模式探测（只测独占，快一倍）；
//! * `-h` / `--help`：打印帮助。

#[cfg(windows)]
fn main() {
    match run() {
        Ok(()) => {}
        Err(error) => {
            eprintln!("format_probe 失败：{error}");
            std::process::exit(1);
        }
    }
}

#[cfg(windows)]
fn run() -> anyhow::Result<()> {
    use stereo_audio::format_support::{
        CANDIDATE_BIT_DEPTHS, CANDIDATE_CHANNELS, CANDIDATE_SAMPLERATES,
    };
    use stereo_audio::{list_endpoints, DataFlow};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut all = false;
    let mut endpoint_arg: Option<String> = None;
    let mut include_shared = true;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--all" => all = true,
            "--endpoint" => {
                index += 1;
                match args.get(index) {
                    Some(value) => endpoint_arg = Some(value.clone()),
                    None => {
                        eprintln!("--endpoint 缺少参数");
                        print_usage();
                        std::process::exit(2);
                    }
                }
            }
            "--no-shared" => include_shared = false,
            "-h" | "--help" => {
                print_usage();
                return Ok(());
            }
            other => {
                eprintln!("未知参数：{other}");
                print_usage();
                std::process::exit(2);
            }
        }
        index += 1;
    }

    if !all && endpoint_arg.is_none() {
        print_usage();
        return Ok(());
    }

    let total = CANDIDATE_SAMPLERATES.len() * CANDIDATE_CHANNELS.len() * CANDIDATE_BIT_DEPTHS.len();
    println!("== 设备真实支持的输出格式（独占模式 IsFormatSupported）==");
    println!(
        "候选矩阵：{} 采样率 × {} 声道 × {} 位深 = {total} 个",
        CANDIDATE_SAMPLERATES.len(),
        CANDIDATE_CHANNELS.len(),
        CANDIDATE_BIT_DEPTHS.len()
    );

    let targets = match endpoint_arg {
        Some(arg) if !all => vec![resolve_target(&arg)?],
        _ => list_endpoints()?
            .into_iter()
            .filter(|endpoint| matches!(endpoint.flow, DataFlow::Render))
            .collect(),
    };

    for endpoint in &targets {
        probe_endpoint(endpoint, include_shared);
    }

    println!();
    println!("共 {} 个端点", targets.len());
    Ok(())
}

/// 探测一个端点并打印全部信息（单端点失败不影响 `--all` 的其它端点）。
#[cfg(windows)]
fn probe_endpoint(endpoint: &stereo_audio::EndpointInfo, include_shared: bool) {
    use stereo_audio::format_support::{
        format_list_summary, format_supported, shared_format_supported, shared_formats,
        supported_formats,
    };
    use stereo_audio::get_endpoint_format;
    use std::time::Instant;

    println!();
    println!("== {} ==", endpoint.name);
    println!("   id: {}", endpoint.id);
    match &endpoint.mix_format {
        Some(mix) => println!("   混音格式: {}", mix.describe()),
        None => println!(
            "   混音格式: <读不到> {}",
            endpoint.error.as_deref().unwrap_or("")
        ),
    }
    let current = get_endpoint_format(&endpoint.id);
    match &current {
        Ok(format) => println!("   设备格式: {}", format.describe()),
        Err(error) => println!("   设备格式: <读取失败> {error}"),
    }

    let started = Instant::now();
    let exclusive = match supported_formats(&endpoint.id) {
        Ok(list) => {
            println!(
                "   独占支持: {}/90 个（{:.0} ms）",
                list.len(),
                started.elapsed().as_secs_f64() * 1000.0
            );
            println!("     {}", format_list_summary(&list));
            Some(list)
        }
        Err(error) => {
            println!("   独占探测失败：{error}");
            None
        }
    };

    if include_shared {
        let started = Instant::now();
        match shared_formats(&endpoint.id) {
            Ok(list) => {
                println!(
                    "   共享可用: {}/90 个（{:.0} ms）",
                    list.len(),
                    started.elapsed().as_secs_f64() * 1000.0
                );
                println!("     {}", format_list_summary(&list));
            }
            Err(error) => println!("   共享探测失败：{error}"),
        }
    }

    if let (Some(list), Ok(format)) = (&exclusive, &current) {
        // 1) 直接拿当前格式去探测（最权威）；
        // 2) 再看它是否出现在矩阵列表里。按关键字段比：设备读回的 24-in-24
        //    与我们请求的 24-in-32 容器写法不同，但两者应是同一能力。
        let direct = format_supported(&endpoint.id, format);
        let in_list = list.iter().any(|candidate| {
            candidate.samplerate == format.samplerate
                && candidate.channels == format.channels
                && candidate.valid_bits == format.valid_bits
                && candidate.is_float == format.is_float
        });
        println!(
            "   当前设备格式独占支持: {}（直接探测）",
            match direct {
                Ok(true) => "是",
                Ok(false) => "否",
                Err(_) => "探测失败",
            }
        );
        println!("   当前设备格式在支持列表内: {}", yes_no(in_list));

        if include_shared {
            let shared_direct = shared_format_supported(&endpoint.id, format);
            println!(
                "   当前设备格式共享可用: {}（直接探测）",
                match shared_direct {
                    Ok(true) => "是",
                    Ok(false) => "否",
                    Err(_) => "探测失败",
                }
            );
        }
    }
}

#[cfg(windows)]
fn resolve_target(arg: &str) -> anyhow::Result<stereo_audio::EndpointInfo> {
    use stereo_audio::{default_endpoint, resolve_endpoint, CaptureMode, DeviceSelector};

    let want = arg.trim();
    if want.is_empty() || want.eq_ignore_ascii_case("default") {
        return Ok(default_endpoint()?);
    }
    match resolve_endpoint(
        &DeviceSelector::NameContains(want.to_owned()),
        CaptureMode::Loopback,
    ) {
        Ok(endpoint) => Ok(endpoint),
        Err(_) => Ok(resolve_endpoint(
            &DeviceSelector::Id(want.to_owned()),
            CaptureMode::Loopback,
        )?),
    }
}

#[cfg(windows)]
fn yes_no(value: bool) -> &'static str {
    if value {
        "是"
    } else {
        "否"
    }
}

#[cfg(windows)]
fn print_usage() {
    println!(
        "\
用法：
  cargo run -q -p stereo-audio --example format_probe -- --endpoint \"Xiaomi Sound-1695\"
  cargo run -q -p stereo-audio --example format_probe -- --endpoint default
  cargo run -q -p stereo-audio --example format_probe -- --all

参数：
  --endpoint <id|友好名子串|default>  只探测一个端点（先按 id，再按友好名子串，default = 默认播放端点）
  --all                               遍历本机所有活动 render 端点
  --no-shared                         跳过共享模式探测（只测独占，快一倍）
  -h, --help                          显示本帮助

说明：独占模式 IsFormatSupported 才反映设备真实能力（S_OK = 支持）；
共享模式只作「能否直接出声」参考（S_OK / S_FALSE 都算可用）。"
    );
}

#[cfg(not(windows))]
fn main() {
    println!("format_probe 只能在 Windows 上运行（需要 WASAPI）");
}
