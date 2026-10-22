//! miplay-recv —— 把本机声卡变成「小米妙播」的投放目标（手机 → 电脑出声）。
//!
//! 直接链接 FusionPlay MiPlay SDK（AGPL-3.0-only）：SDK 内部完成 Lyra / mDNS
//! 播报、MiPlay 控制面、RTSP/RTP 接收、解密、AAC 解码，并直接把 PCM 写进
//! cpal 输出设备——所以这个 exe 只需要「启动 + 选声卡」，不需要把 PCM 回传给
//! 任何别的进程。
//!
//! 本 crate 因链接 AGPL 代码，整体按 AGPL-3.0-only 发布；StereoLink 主程序
//! （MIT OR Apache-2.0）不链接它，两者只在「声音从本机声卡出来」这一点上解耦，
//! 进程之间没有任何 IPC，也不共享代码。

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait};
use fusionplay_miplay_sdk::{
    EventEmitter, MiPlayDeviceType, MiPlayReceiver, ReceiverConfig, ReceiverController,
    ReceiverIdentity,
};
use rand::RngCore;
use serde_json::json;
use std::io::{BufRead, Write};
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const DEFAULT_NAME: &str = "StereoLink";
/// 与 airplay-recv 保持同一枚本地管理地址（02 开头 = 本地管理单播）。
const DEFAULT_MAC: &str = "02:53:54:52:4C:4E";
/// 状态文件里没记录过音量、命令行也没给 `--volume` 时的默认值。
const DEFAULT_VOLUME: u8 = 50;

fn main() -> Result<()> {
    let args = Args::parse()?;
    init_tracing(args.verbose);

    // 妙播的上游 SDK 把「目标延迟」写死成 800 ms（排队至少 650 ms），只投本机时会
    // 比手机/音箱明显慢半拍。vendor 副本支持用环境变量覆盖，这里把命令行传下去。
    // 默认不设 = 保持上游的 800/650，和小米全屋播放对齐。
    if let Some(ms) = args.group_delay_ms {
        std::env::set_var("FUSIONPLAY_MIPLAY_GROUP_DELAY_MS", ms.to_string());
    }
    if let Some(ms) = args.min_queue_ms {
        std::env::set_var("FUSIONPLAY_MIPLAY_MIN_QUEUE_MS", ms.to_string());
    }

    if args.list_devices {
        return list_output_devices();
    }

    let ip = match args.ip {
        Some(ip) => {
            if ip.is_loopback() || ip.is_unspecified() {
                bail!("--ip {ip} 不是局域网地址");
            }
            ip
        }
        None => local_ipv4().context(
            "无法自动识别本机局域网 IPv4 地址，请用 --ip <192.168.x.x> 指定",
        )?,
    };

    // --device 支持 #序号：WASAPI 端点常常重名，靠序号/id 选最省事。
    let device = match args.device.as_deref() {
        Some(want) => Some(resolve_output_device(want)?),
        None => None,
    };
    // 状态文件同时存设备身份和上次的音量：删掉会生成一台新设备，音量回到默认值。
    let state_path = args.state.clone();
    let identity = Identity::load_or_generate(&state_path);
    let initial_volume = effective_volume(args.volume, load_saved_volume(&state_path));
    // 启动时就把生效值写回：旧版状态文件在这里补上 volume 字段，显式 --volume 也会被记住。
    let volume_store = Arc::new(Mutex::new(VolumeStore {
        path: state_path,
        identity: identity.clone(),
        volume: initial_volume,
    }));
    if let Ok(store) = volume_store.lock() {
        store.save();
    }

    let name = args.name.clone();
    let verbose = args.verbose;
    let quiet = args.quiet;
    let event_store = Arc::clone(&volume_store);
    let events: EventEmitter = Arc::new(move |value: serde_json::Value| {
        let event = value
            .get("event")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_owned();
        // 每个音频包都会发一条 audio_pcm_decoded，直接丢掉，避免刷屏。
        if event == "audio_pcm_decoded" {
            return;
        }
        // 发现/播报类事件只在 -v 时打：正常使用时它们一秒好几条，会淹没有用信息。
        if !verbose && event.starts_with("discovery_") {
            return;
        }
        // --quiet：由面板/后台脚本拉起时用，只保留报错类事件和 volume 事件，
        // 其余交给界面去显示状态。
        if quiet && !verbose && !is_problem_event(&event) && event != "volume" {
            return;
        }
        // 手机端调音量（协议 SET_VOLUME / GET_VOLUME）→ SDK 发 volume 事件。
        // 这里先落盘再打印：脚本/面板看到这一行时，状态文件里已经是最新值。
        if event == "volume" {
            let percent = value
                .get("percent")
                .and_then(|v| v.as_u64())
                .map_or(0, |percent| percent.min(100) as u8);
            if let Ok(mut store) = event_store.lock() {
                store.set(percent);
            }
            print_volume("xiaomi", percent);
            return;
        }
        let mut detail = value.clone();
        if let Some(map) = detail.as_object_mut() {
            map.remove("event");
        }
        let mut text = serde_json::to_string(&detail).unwrap_or_default();
        if text.len() > 320 {
            text.truncate(320);
            text.push('…');
        }
        println!("[妙播] {event} {text}");
        let _ = std::io::stdout().flush();
    });

    let config = ReceiverConfig::new(
        name.clone(),
        ReceiverIdentity::new(
            identity.idm_instance_id.clone(),
            identity.lyra_instance_id.clone(),
            identity.media_device_id.clone(),
        ),
        ip,
        args.iface.clone().unwrap_or_default(),
    )
    .with_device_type(args.device_type)
    .with_output_device(device.clone())
    .with_initial_volume_percent(u32::from(initial_volume))
    // 仅供本机自检：让 SDK 的响应 socket 打开 IP_MULTICAST_LOOP，
    // 这样同一台机器上发出的探测 query 也能被自己收到并回包。
    .with_diagnostic_multicast_loopback(args.diag_loopback);

    println!("📡 本机地址：{ip}");
    match &device {
        Some(device) => println!("🔈 输出声卡：{device}（找不到时自动回落到系统默认声卡）"),
        None => {
            // 面板要显示「手机声音到底出在哪块声卡」：这里把系统默认输出设备读出来，
            // 光写「系统默认」用户看不出是哪块。
            let named = cpal::default_host()
                .default_output_device()
                .map(|d| describe(&d));
            match named {
                Some(name) => println!("🔈 输出声卡：{name}（系统默认）"),
                None => println!("🔈 输出声卡：系统默认输出设备"),
            }
        }
    }
    println!("🔊 接收端音量：{initial_volume}（手机端可调，面板/CLI 也能改）");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("无法创建 tokio 运行时")?;

    runtime.block_on(async move {
        let receiver =
            MiPlayReceiver::start(config, Arc::clone(&events)).context("妙播接收端启动失败")?;

        // 面板/CLI 通过 stdin 实时改音量：一行一条命令，独立线程处理，不阻塞 tokio 主循环。
        // stdin 被关闭（EOF）时只结束这个线程，接收端继续工作。
        let controller = receiver.controller();
        let control_store = Arc::clone(&volume_store);
        std::thread::spawn(move || stdin_control_loop(controller, control_store));

        println!();
        println!("✅ 小米妙播接收端已就绪：「{name}」");
        println!("   Lyra 控制端口：{}", receiver.lyra_command_port());
        println!("   mDNS 服务名：{}", receiver.service_name());
        if args.group_delay_ms.is_some() || args.min_queue_ms.is_some() {
            println!(
                "   目标延迟：{} ms，排队下限：{} ms{}",
                args.group_delay_ms.unwrap_or(800),
                args.min_queue_ms.unwrap_or(650),
                if args.group_delay_ms.unwrap_or(800) < 650 {
                    "（低延迟模式：和小米音箱一起全屋播放会不同步）"
                } else {
                    ""
                }
            );
        }
        println!("   现在打开手机「小米妙播」（控制中心 / 音乐 App 的投送按钮），选择「{name}」");
        println!("   ⚠️ 需要保持本窗口开着；如果手机看不到设备，先放行 Windows 防火墙");
        println!("   按 Ctrl+C 停止");
        println!();
        let _ = std::io::stdout().flush();

        let stop_signal = tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
        });
        // SDK 的 start() 已经在后台线程里跑控制面/媒体面，这里只需要等退出信号。
        let _ = stop_signal.await;

        println!();
        println!("👋 正在停止妙播接收端…");
        receiver.controller().stop();
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}

// ---------------------------------------------------------------- 参数

struct Args {
    name: String,
    device: Option<String>,
    ip: Option<Ipv4Addr>,
    iface: Option<String>,
    state: PathBuf,
    /// 接收端音量初值；不写就用状态文件里上次保存的值（都没有则 50）。
    volume: Option<u8>,
    device_type: MiPlayDeviceType,
    /// 妙播目标延迟（毫秒）：越小越跟手，但和小米音箱一起全屋播放时会不同步。
    group_delay_ms: Option<u64>,
    /// 排队延迟下限（毫秒）。
    min_queue_ms: Option<u64>,
    list_devices: bool,
    diag_loopback: bool,
    quiet: bool,
    verbose: bool,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = Args {
            name: DEFAULT_NAME.to_owned(),
            device: None,
            ip: None,
            iface: None,
            state: default_state_path(),
            volume: None,
            device_type: MiPlayDeviceType::Speaker,
            group_delay_ms: None,
            min_queue_ms: None,
            list_devices: false,
            diag_loopback: false,
            quiet: false,
            verbose: false,
        };

        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--name" | "-n" => args.name = value(&mut iter, &arg)?,
                "--device" | "-d" => args.device = Some(value(&mut iter, &arg)?),
                "--ip" => {
                    let raw = value(&mut iter, &arg)?;
                    args.ip = Some(
                        raw.parse()
                            .with_context(|| format!("无法解析 --ip {raw}"))?,
                    );
                }
                "--iface" => args.iface = Some(value(&mut iter, &arg)?),
                "--state" => args.state = PathBuf::from(value(&mut iter, &arg)?),
                "--volume" => {
                    let raw = value(&mut iter, &arg)?;
                    args.volume = Some(
                        raw.parse::<u32>()
                            .with_context(|| format!("--volume 需要 0-100，收到 {raw}"))?
                            .min(100) as u8,
                    );
                }
                "--type" => {
                    let raw = value(&mut iter, &arg)?;
                    args.device_type = match raw.to_ascii_lowercase().as_str() {
                        "speaker" => MiPlayDeviceType::Speaker,
                        "tv" | "television" => MiPlayDeviceType::Television,
                        "display" | "display_speaker" => MiPlayDeviceType::DisplaySpeaker,
                        "tablet" => MiPlayDeviceType::Tablet,
                        "vehicle" => MiPlayDeviceType::Vehicle,
                        _ => bail!("--type 只支持 speaker / tv / display / tablet / vehicle"),
                    };
                }
                "--group-delay-ms" => {
                    let raw = value(&mut iter, &arg)?;
                    args.group_delay_ms = Some(
                        raw.parse::<u64>()
                            .with_context(|| format!("--group-delay-ms 需要毫秒整数，收到 {raw}"))?,
                    );
                }
                "--min-queue-ms" => {
                    let raw = value(&mut iter, &arg)?;
                    args.min_queue_ms = Some(
                        raw.parse::<u64>()
                            .with_context(|| format!("--min-queue-ms 需要毫秒整数，收到 {raw}"))?,
                    );
                }
                "--list-devices" => args.list_devices = true,
                "--diag-loopback" => args.diag_loopback = true,
                "--quiet" => args.quiet = true,
                "-v" | "--verbose" => args.verbose = true,
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => bail!("未知参数：{other}（用 --help 查看用法）"),
            }
        }

        if args.name.trim().is_empty() {
            bail!("--name 不能为空");
        }
        Ok(args)
    }
}

fn value<I: Iterator<Item = String>>(iter: &mut I, flag: &str) -> Result<String> {
    iter.next()
        .with_context(|| format!("{flag} 缺少取值"))
}

fn print_help() {
    println!(
        r#"miplay-recv —— 把本机声卡变成小米妙播（MiPlay）的投放目标

用法：
  miplay-recv [选项]

选项：
  -n, --name <名字>      投放列表里显示的名字（默认 {DEFAULT_NAME}）
  -d, --device <声卡>    输出声卡：#序号 / cpal 设备 id / 设备名（不区分大小写）
                         不填 = 系统默认输出设备（推荐）
      --ip <IPv4>        本机局域网地址；不填则自动探测
      --iface <网卡名>   Windows 网卡友好名（一般不用填，SDK 会用 --ip 自己反查）
      --type <类型>      speaker（默认）/ tv / display / tablet / vehicle
      --volume <0-100>   接收端音量初值（不写就用上次保存的，默认 50）
      --state <文件>     身份与音量持久化文件（默认 %LOCALAPPDATA%\StereoLink\miplay-recv.json）
      --list-devices     列出本机输出设备后退出
      --diag-loopback    仅自检用：允许本机发出的 mDNS 探测被自己收到
      --group-delay-ms <ms>  妙播目标延迟（默认 800）。越小越跟手：
                         200~300 已经够听音乐；设小了和小米音箱一起
                         「全屋播放」会不同步。
      --min-queue-ms <ms>    排队延迟下限（默认 650），一般只在改上面那个时一起调小
      --quiet            只打印报错类事件和 volume 事件（StereoLink 面板拉起本程序时使用）
  -v, --verbose          打开 SDK 的调试日志
  -h, --help             显示本帮助

stdin 控制（面板/脚本用管道喂，一行一条命令，UTF-8）：
  volume <0-100>         设置接收端音量：会通知手机端，并记到状态文件（重启后保持）
                         成功打印 [妙播] volume {{"source":"host","percent":N}}
                         非法值/未知命令打印 [妙播] control_error，进程不退出

说明：
  本程序直接链接 FusionPlay MiPlay SDK（AGPL-3.0-only），自己完成妙播发现、
  控制面、RTP 接收与 AAC 解码，并把 PCM 直接写进本机声卡。
  它和 StereoLink 主程序没有任何 IPC：只要这个窗口在跑，手机就能把音乐投过来。
"#
    );
}

// ---------------------------------------------------------------- 身份与音量状态

#[derive(Clone)]
struct Identity {
    idm_instance_id: String,
    lyra_instance_id: String,
    media_device_id: String,
}

impl Identity {
    /// 读状态文件里的设备身份；文件不存在/损坏/三个字段不全时返回 None。
    fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let value = serde_json::from_str::<serde_json::Value>(&text).ok()?;
        let read = |key: &str| {
            value
                .get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
        };
        match (
            read("idm_instance_id"),
            read("lyra_instance_id"),
            read("media_device_id"),
        ) {
            (Some(idm_instance_id), Some(lyra_instance_id), Some(media_device_id)) => Some(Self {
                idm_instance_id,
                lyra_instance_id,
                media_device_id,
            }),
            _ => None,
        }
    }

    fn generate() -> Self {
        Self {
            idm_instance_id: random_uuid(),
            lyra_instance_id: random_hex_upper(4),
            media_device_id: DEFAULT_MAC.to_owned(),
        }
    }

    fn load_or_generate(path: &Path) -> Self {
        Self::load(path).unwrap_or_else(Self::generate)
    }
}

/// 状态文件里的 `volume` 字段。旧版文件没有这个字段 → None，由调用方回落到默认 50。
fn load_saved_volume(path: &Path) -> Option<u8> {
    let text = std::fs::read_to_string(path).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    value
        .get("volume")
        .and_then(|v| v.as_u64())
        .map(|percent| percent.min(100) as u8)
}

/// 生效音量：显式 `--volume` > 状态文件里上次保存的 > 默认 50。
fn effective_volume(explicit: Option<u8>, saved: Option<u8>) -> u8 {
    explicit.or(saved).unwrap_or(DEFAULT_VOLUME).min(100)
}

/// 把身份和当前音量写回状态文件；读写失败只忽略，不打断接收。
fn save_state(path: &Path, identity: &Identity, volume: u8) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let payload = json!({
        "idm_instance_id": identity.idm_instance_id,
        "lyra_instance_id": identity.lyra_instance_id,
        "media_device_id": identity.media_device_id,
        "volume": volume.min(100),
        "note": "妙播接收端身份与音量；删掉会变成一台新设备（手机需要重新选择投放目标），音量回到默认 50",
    });
    if let Ok(text) = serde_json::to_string_pretty(&payload) {
        let _ = std::fs::write(path, text);
    }
}

/// 进程内共享的音量状态：stdin 命令（source=host）与手机端 volume 事件（source=xiaomi）
/// 都写这里，每次变更立即落盘，重启后按上次的值启动。
struct VolumeStore {
    path: PathBuf,
    identity: Identity,
    volume: u8,
}

impl VolumeStore {
    fn save(&self) {
        save_state(&self.path, &self.identity, self.volume);
    }

    fn set(&mut self, percent: u8) {
        self.volume = percent.min(100);
        self.save();
    }
}

fn default_state_path() -> PathBuf {
    match std::env::var_os("LOCALAPPDATA") {
        Some(base) => PathBuf::from(base).join("StereoLink").join("miplay-recv.json"),
        None => PathBuf::from("miplay-recv.json"),
    }
}

// ---------------------------------------------------------------- stdin 控制通道

/// 解析一行 stdin 命令。成功返回音量；失败返回契约里的原因：
/// `unknown_command`（不认识的命令）或 `bad_value`（volume 取值非法）。
fn parse_volume_command(line: &str) -> Result<u8, &'static str> {
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("volume") => {}
        _ => return Err("unknown_command"),
    }
    let raw = parts.next().ok_or("bad_value")?;
    // 多余参数按非法处理，避免脚本把 `volume 30 40` 里的 40 当静默忽略。
    if parts.next().is_some() {
        return Err("bad_value");
    }
    match raw.parse::<u8>() {
        Ok(percent) if percent <= 100 => Ok(percent),
        _ => Err("bad_value"),
    }
}

/// `[妙播] volume {...}` 是机器可读协议行（字段顺序固定，面板/脚本按前缀解析）。
fn volume_line(source: &str, percent: u8) -> String {
    format!("[妙播] volume {{\"source\":\"{source}\",\"percent\":{percent}}}")
}

fn control_error_line(reason: &str, line: &str) -> String {
    // 原始行用 JSON 字符串编码，保证引号/反斜杠/中文都不会破坏这一行 JSON。
    let line = serde_json::to_string(line).unwrap_or_else(|_| "\"\"".to_owned());
    format!("[妙播] control_error {{\"reason\":\"{reason}\",\"line\":{line}}}")
}

fn print_protocol_line(line: &str) {
    println!("{line}");
    let _ = std::io::stdout().flush();
}

fn print_volume(source: &str, percent: u8) {
    print_protocol_line(&volume_line(source, percent));
}

fn print_control_error(reason: &str, line: &str) {
    print_protocol_line(&control_error_line(reason, line));
}

/// stdin 命令线程：一行一条命令，`volume <0-100>` 实时改音量并落盘。
/// 空行忽略；stdin EOF（面板关掉管道）只结束本线程，接收端继续工作等 Ctrl+C。
fn stdin_control_loop(controller: ReceiverController, store: Arc<Mutex<VolumeStore>>) {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) => {
                tracing::warn!("[妙播] 读取 stdin 控制命令失败：{error}");
                return;
            }
        }
        let command = line.trim();
        if command.is_empty() {
            continue;
        }
        match parse_volume_command(command) {
            Ok(percent) => {
                // 先落盘再打印：面板/脚本看到 volume 行时状态文件里已经是最新值。
                if let Ok(mut guard) = store.lock() {
                    guard.set(percent);
                }
                // 没有手机连接时 SDK 无处发送通知（返回错误），但值已经记住：
                // 下一次启动会以它作为 initial_volume_percent 生效。
                if let Err(error) = controller.set_volume(percent) {
                    tracing::warn!(
                        "[妙播] 当前没有已连接的小米妙播设备，音量 {percent} 已保存，下次启动生效：{error}"
                    );
                }
                print_volume("host", percent);
            }
            Err(reason) => print_control_error(reason, command),
        }
    }
}

fn random_hex_upper(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buffer);
    buffer.iter().map(|b| format!("{b:02X}")).collect()
}

fn random_uuid() -> String {
    let mut buffer = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buffer);
    // RFC 4122 v4 形状，不必严格（SDK 只要求稳定字符串）。
    buffer[6] = (buffer[6] & 0x0f) | 0x40;
    buffer[8] = (buffer[8] & 0x3f) | 0x80;
    let hex: String = buffer.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

// ---------------------------------------------------------------- 网络 / 声卡

fn local_ipv4() -> Result<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").context("无法创建探测 socket")?;
    socket
        .connect("223.5.5.5:80")
        .context("无法探测默认出网地址")?;
    match socket.local_addr().context("无法读取本地地址")?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Ok(ip),
        other => bail!("自动探测到的地址 {other} 不可用，请用 --ip 指定"),
    }
}

fn describe(device: &cpal::Device) -> String {
    let name = device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| "<未知>".to_owned());
    let id = device.id().map(|id| id.to_string()).unwrap_or_default();
    let extra = device
        .default_output_config()
        .map(|config| {
            format!(
                "{} Hz / {} ch / {:?}",
                config.sample_rate(),
                config.channels(),
                config.sample_format()
            )
        })
        .unwrap_or_else(|_| "无法读取默认格式".to_owned());
    format!("{name}｜{extra}｜id={id}")
}

fn device_id(device: &cpal::Device) -> Option<String> {
    device.id().ok().map(|id| id.to_string())
}

/// 把 `--device` 的值翻译成 SDK 认识的字符串。
/// SDK 只按「设备 id 或设备名」匹配，所以 `#N` 要先在这里换算成 id。
fn resolve_output_device(want: &str) -> Result<String> {
    let Some(rest) = want.strip_prefix('#') else {
        return Ok(want.to_owned());
    };
    let index: usize = rest
        .parse()
        .with_context(|| format!("{want} 不是合法序号，例如 #3"))?;
    let host = cpal::default_host();
    let device = host
        .output_devices()
        .context("无法枚举输出设备")?
        .nth(index.saturating_sub(1))
        .ok_or_else(|| anyhow::anyhow!("没有第 {index} 个输出设备（用 --list-devices 看序号）"))?;
    match device_id(&device) {
        Some(id) => Ok(id),
        None => device
            .description()
            .map(|d| d.name().to_owned())
            .map_err(|error| anyhow::anyhow!("第 {index} 个输出设备既没有 id 也读不到名字：{error}")),
    }
}

fn list_output_devices() -> Result<()> {
    let host = cpal::default_host();
    let default_id = host.default_output_device().and_then(|device| device_id(&device));

    println!("本机输出设备（--device 写 #序号 或 id；名字精确匹配也行）：");
    let devices: Vec<_> = host
        .output_devices()
        .context("无法枚举输出设备")?
        .collect();
    for (index, device) in devices.iter().enumerate() {
        let marker = if device_id(device) == default_id {
            " ← 系统默认"
        } else {
            ""
        };
        println!("  [{}] {}{marker}", index + 1, describe(device));
    }
    println!();
    println!("用法：--device \"#1\"（序号）或 --device \"<id>\"（最稳，重名也不会选错）");
    Ok(())
}

/// `--quiet` 下仍然要显示的「有事发生」事件：出错了才值得占用面板输出。
fn is_problem_event(event: &str) -> bool {
    let event = event.to_ascii_lowercase();
    ["error", "fail", "denied", "stall", "unsupported", "closed"]
        .iter()
        .any(|needle| event.contains(needle))
}

fn init_tracing(verbose: bool) {
    let filter = if verbose {
        "info,fusionplay_miplay_sdk=debug,cpal=info"
    } else {
        "warn"
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| filter.parse().unwrap()),
        )
        .with_ansi(false)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "miplay-recv-test-{}-{}-{}",
            std::process::id(),
            tag,
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).expect("创建测试临时目录");
        dir.join("miplay-recv.json")
    }

    fn cleanup(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn old_state_file_without_volume_falls_back_to_50() {
        let path = temp_state_path("old");
        std::fs::write(
            &path,
            r#"{"idm_instance_id":"idm-1","lyra_instance_id":"ABCD1234","media_device_id":"02:53:54:52:4C:4E"}"#,
        )
        .unwrap();

        // 旧版状态文件没有 volume 字段：读回 None，生效值回落到默认 50。
        assert_eq!(load_saved_volume(&path), None);
        assert_eq!(effective_volume(None, load_saved_volume(&path)), 50);
        // 身份字段仍然可用（升级不会把设备变成一台新的）。
        let identity = Identity::load(&path).expect("旧文件的身份字段应能读出");
        assert_eq!(identity.lyra_instance_id, "ABCD1234");

        // 写入 volume 后立即可读回，且身份字段不丢。
        save_state(&path, &identity, 30);
        assert_eq!(load_saved_volume(&path), Some(30));
        assert_eq!(effective_volume(None, load_saved_volume(&path)), 30);
        // 显式 --volume 优先于状态文件。
        assert_eq!(effective_volume(Some(20), load_saved_volume(&path)), 20);
        let reloaded = Identity::load(&path).expect("保存后身份字段应保留");
        assert_eq!(reloaded.idm_instance_id, "idm-1");
        assert_eq!(reloaded.media_device_id, "02:53:54:52:4C:4E");

        // 0 是合法音量，不能被当成「没保存」。
        save_state(&path, &identity, 0);
        assert_eq!(load_saved_volume(&path), Some(0));
        assert_eq!(effective_volume(None, load_saved_volume(&path)), 0);
        // 兜底：超出范围的值按 100 截断。
        assert_eq!(effective_volume(Some(150), None), 100);
        cleanup(&path);
    }

    #[test]
    fn parse_volume_command_accepts_zero_to_hundred_only() {
        assert_eq!(parse_volume_command("volume 30"), Ok(30));
        assert_eq!(parse_volume_command("volume 0"), Ok(0));
        assert_eq!(parse_volume_command("volume 100"), Ok(100));
        assert_eq!(parse_volume_command("  volume   42  "), Ok(42));

        assert_eq!(parse_volume_command("volume 101"), Err("bad_value"));
        assert_eq!(parse_volume_command("volume 150"), Err("bad_value"));
        assert_eq!(parse_volume_command("volume -1"), Err("bad_value"));
        assert_eq!(parse_volume_command("volume abc"), Err("bad_value"));
        assert_eq!(parse_volume_command("volume"), Err("bad_value"));
        assert_eq!(parse_volume_command("volume 30 40"), Err("bad_value"));

        assert_eq!(parse_volume_command("play 30"), Err("unknown_command"));
        assert_eq!(parse_volume_command("Volume 30"), Err("unknown_command"));
        assert_eq!(parse_volume_command(""), Err("unknown_command"));
    }

    #[test]
    fn protocol_lines_match_frozen_contract() {
        assert_eq!(
            volume_line("host", 30),
            r#"[妙播] volume {"source":"host","percent":30}"#
        );
        assert_eq!(
            volume_line("xiaomi", 100),
            r#"[妙播] volume {"source":"xiaomi","percent":100}"#
        );
        assert_eq!(
            control_error_line("unknown_command", "play"),
            r#"[妙播] control_error {"reason":"unknown_command","line":"play"}"#
        );
        // 原始行带引号/反斜杠时必须仍是合法 JSON（转义由 serde_json 负责）。
        assert_eq!(
            control_error_line("bad_value", r#"volume "abc\"#),
            r#"[妙播] control_error {"reason":"bad_value","line":"volume \"abc\\"}"#
        );
    }
}
