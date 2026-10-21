//! 把本机声卡变成 AirPlay（隔空播放）接收端。
//!
//! 手机（iPhone 的隔空播放，或 macOS 的声音输出）选中这台电脑后，音频会通过
//! RAOP 传过来，由本程序直接交给本机声卡出声——不经过 StereoLink 的左右声道
//! 引擎，两边完全解耦。
//!
//! ```text
//! cargo run --release -p airplay-recv -- --name "电脑（StereoLink）"
//! ```

mod audio;

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use shairplay::{AudioFormat, AudioHandler, AudioSession, BindConfig, RaopServer};

/// 设备 MAC。mDNS 的 `_raop._tcp` 记录里要带，AirPlay 靠它区分设备。
/// 首位 0x02 = 本地管理地址、单播位，是免冲突的自定义 MAC 常用取值。
const DEVICE_MAC: [u8; 6] = [0x02, 0x53, 0x54, 0x52, 0x4c, 0x4e];

/// 默认采样率。AirPlay 1 的 ALAC 基本都是 44.1 kHz，开在同一个速率上可以完全
/// 不重采样；声卡混音格式是 48 kHz 时由 Windows 自己转。
const DEFAULT_RATE: u32 = 44_100;
const DEFAULT_PORT: u16 = 5000;

#[derive(Debug)]
struct Args {
    name: String,
    port: u16,
    rate: u32,
    device: Option<String>,
    bind: Vec<IpAddr>,
    list_devices: bool,
    verbose: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            name: "StereoLink".to_string(),
            port: DEFAULT_PORT,
            rate: DEFAULT_RATE,
            device: None,
            bind: Vec::new(),
            list_devices: false,
            verbose: false,
        }
    }
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = Self::default();
        let argv: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < argv.len() {
            match argv[i].as_str() {
                "--name" | "-n" => {
                    args.name = value_at(&argv, i, "--name")?;
                    i += 1;
                }
                "--port" | "-p" => {
                    args.port = value_at(&argv, i, "--port")?
                        .parse()
                        .context("--port 要是一个 1..65535 的整数")?;
                    i += 1;
                }
                "--rate" | "-r" => {
                    args.rate = value_at(&argv, i, "--rate")?
                        .parse()
                        .context("--rate 要是一个整数，例如 44100")?;
                    i += 1;
                }
                "--device" | "-d" => {
                    args.device = Some(value_at(&argv, i, "--device")?);
                    i += 1;
                }
                "--bind" => {
                    let ip = value_at(&argv, i, "--bind")?;
                    args.bind
                        .push(ip.parse().with_context(|| format!("--bind {ip} 不是合法 IP"))?);
                    i += 1;
                }
                "--list-devices" => args.list_devices = true,
                "-v" | "--verbose" => args.verbose = true,
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => bail!("看不懂的参数：{other}\n用 --help 看看支持哪些"),
            }
            i += 1;
        }
        Ok(args)
    }
}

fn value_at(argv: &[String], i: usize, name: &str) -> Result<String> {
    argv.get(i + 1)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{name} 后面要跟一个值"))
}

fn print_help() {
    println!(
        "\
airplay-recv —— 把本机声卡变成 AirPlay（隔空播放）接收端

用法:
  airplay-recv [选项]

选项:
  -n, --name <名字>      手机「隔空播放」列表里显示的名字（默认 StereoLink）
  -p, --port <端口>      RTSP 端口（默认 {DEFAULT_PORT}；被占用会自动往后找）
  -r, --rate <采样率>    输出采样率（默认 {DEFAULT_RATE}，和 AirPlay 音源一致就不用重采样）
  -d, --device <声卡>    输出到哪个声卡：名字子串 / 完整 id / #序号（默认 default）
      --bind <IP>        只监听某个网卡（可重复）
      --list-devices     列出本机输出设备后退出
  -v, --verbose          打开调试日志
  -h, --help             显示这段帮助"
    );
}

struct Handler {
    output: audio::Output,
}

impl AudioHandler for Handler {
    fn audio_init(&self, format: AudioFormat) -> Box<dyn AudioSession> {
        Box::new(audio::Session::new(self.output.clone(), &format))
    }

    fn on_volume(&self, volume: f32) {
        self.output.set_volume_db(volume);
        tracing::debug!(volume, "音量变化");
    }

    fn on_metadata(&self, metadata: &shairplay::TrackMetadata) {
        // DMAP 元数据是 Variable-length 的，字段名随发送端而变；Debug 打印最稳。
        tracing::info!(?metadata, "曲目信息");
    }

    fn on_client_connected(&self, addr: &str) {
        println!("📱 手机已连接：{addr}");
    }

    fn on_client_disconnected(&self, addr: &str) {
        println!("📴 手机断开：{addr}");
    }

    fn on_error(&self, error: &shairplay::ShairplayError) {
        tracing::warn!(%error, "AirPlay 运行时报错");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse()?;
    init_tracing(args.verbose);

    if args.list_devices {
        return audio::list_devices();
    }

    let (output, _stream, device_name) =
        audio::Output::open(args.device.as_deref(), args.rate, None)?;
    println!(
        "🔈 输出声卡：{device_name}（{} Hz / {} 声道）",
        output.rate(),
        output.channels()
    );

    let handler = Arc::new(Handler {
        output: output.clone(),
    });

    let mut builder = RaopServer::builder()
        .name(args.name.clone())
        .hwaddr(DEVICE_MAC)
        // 只允许一路客户端：两个手机同时投会把两首歌混在一起，没有意义。
        .max_clients(1)
        .port(args.port);
    if !args.bind.is_empty() {
        builder = builder.bind(BindConfig::new().addrs(args.bind.clone()));
    }

    let mut server = builder.build(handler).context("创建 AirPlay 服务失败")?;
    server.start().await.context("AirPlay 服务启动失败")?;

    // mDNS 注册失败时 start() 可能返回 Ok 但服务其实没在跑，以 is_running 为准。
    if !server.is_running() {
        bail!("AirPlay 服务没能启动（端口被占用，或 mDNS 注册失败）");
    }

    let info = server.service_info();
    println!();
    println!("✅ AirPlay 接收端已就绪：「{}」", info.airplay_name);
    println!("   RTSP 端口：{}", info.port);
    if let Some(ip) = local_ipv4() {
        println!("   本机局域网 IP：{ip}");
    }
    println!("   现在用手机「隔空播放」/ 控制中心的音频输出选择这个名字即可");
    println!("   ⚠️ 需要保持本窗口开着；如果手机看不到设备，先放行 Windows 防火墙");
    println!("   按 Ctrl+C 停止");
    println!();

    tokio::signal::ctrl_c().await.context("等待 Ctrl+C 失败")?;
    println!("\n正在停止…");
    server.stop().await;
    Ok(())
}

fn init_tracing(verbose: bool) {
    use tracing_subscriber::EnvFilter;
    let default = if verbose { "airplay_recv=debug,shairplay=debug" } else { "airplay_recv=info,shairplay=warn" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// 不引第三方库地取本机出口 IP：连一下外网地址，读 socket 的本地端点即可。
fn local_ipv4() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("223.5.5.5:80").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() => Some(IpAddr::V4(v4)),
        _ => None,
    }
}
