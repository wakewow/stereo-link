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
use shairplay::{
    AirPlayMode, AudioFormat, AudioHandler, AudioSession, BindConfig, PairingStore, RaopServer,
};

/// 设备 MAC。mDNS 的 `_raop._tcp` 记录里要带，AirPlay 靠它区分设备。
/// 首位 0x02 = 本地管理地址、单播位，是免冲突的自定义 MAC 常用取值。
const DEVICE_MAC: [u8; 6] = [0x02, 0x53, 0x54, 0x52, 0x4c, 0x4e];

/// 默认采样率。AirPlay 的 ALAC / AAC 基本都是 44.1 kHz，开在同一个速率上可以
/// 完全不重采样；声卡混音格式是 48 kHz 时由 Windows 自己转。
const DEFAULT_RATE: u32 = 44_100;
const DEFAULT_PORT: u16 = 5000;

/// AirPlay 2 的 HomeKit 配对码。固定 3939 并打印在启动横幅里：iPhone 第一次
/// 连这台电脑时输入一次，之后走 pair-verify 快速重连（配对信息落盘）。
const DEFAULT_PIN: &str = "3939";

/// AP2 模式允许的并发 RTSP 连接数。iOS 会同时开「遥控」和「音频」两条连接，
/// 只给 1 条会被 shairplay 直接拒掉（日志 `Max connections reached`）；AP1 只有
/// 一条实时流，保持 1 路避免两台手机同时投混播。
const MAX_CLIENTS_AP2: usize = 4;

/// 按模式取并发连接上限：AP1 只放 1 路，AP2 放宽到 [`MAX_CLIENTS_AP2`]。
fn max_clients(ap1: bool) -> usize {
    if ap1 { 1 } else { MAX_CLIENTS_AP2 }
}

/// 配对信息落盘位置：`%LOCALAPPDATA%\StereoLink\airplay-pairings.json`。
/// 删掉这个文件 = 让所有手机重新配对。
fn pairing_path() -> std::path::PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("StereoLink")
        .join("airplay-pairings.json")
}

/// 落盘内容：本机长期身份种子（让 `pk` 跨重启稳定）+ 已配对设备的公钥。
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct PersistState {
    #[serde(default)]
    identity_seed: Option<[u8; 32]>,
    #[serde(default)]
    paired_keys: std::collections::HashMap<String, [u8; 32]>,
}

impl PersistState {
    fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

/// 把配对状态存到文件的 `PairingStore`。用上它，iPhone 只需配对一次。
struct FilePairingStore {
    path: std::path::PathBuf,
    state: std::sync::Mutex<PersistState>,
}

impl FilePairingStore {
    fn new(path: std::path::PathBuf) -> Self {
        let state = PersistState::load(&path);
        Self {
            path,
            state: std::sync::Mutex::new(state),
        }
    }
}

impl PairingStore for FilePairingStore {
    fn get(&self, device_id: &str) -> Option<[u8; 32]> {
        self.state.lock().ok()?.paired_keys.get(device_id).copied()
    }

    fn put(&self, device_id: &str, public_key: [u8; 32]) {
        if let Ok(mut state) = self.state.lock() {
            state.paired_keys.insert(device_id.to_string(), public_key);
            state.save(&self.path);
        }
    }

    fn remove(&self, device_id: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.paired_keys.remove(device_id);
            state.save(&self.path);
        }
    }

    fn has_any_pairing(&self) -> bool {
        self.state
            .lock()
            .map(|state| !state.paired_keys.is_empty())
            .unwrap_or(false)
    }

    fn load_identity(&self) -> Option<[u8; 32]> {
        self.state.lock().ok()?.identity_seed
    }

    fn save_identity(&self, seed: [u8; 32]) {
        if let Ok(mut state) = self.state.lock() {
            state.identity_seed = Some(seed);
            state.save(&self.path);
        }
    }
}

#[derive(Debug)]
struct Args {
    name: String,
    port: u16,
    rate: u32,
    device: Option<String>,
    bind: Vec<IpAddr>,
    list_devices: bool,
    /// 只走 AirPlay 1（RAOP 实时流）。默认 AP1 + AP2 双支持。
    ap1: bool,
    /// 无 PIN 的临时配对：连得更慢，但手机端不用输配对码。
    transient: bool,
    /// 自定义配对码（默认 3939）。
    pin: Option<String>,
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
            ap1: false,
            transient: false,
            pin: None,
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
                "--ap1" => args.ap1 = true,
                "--transient" => args.transient = true,
                "--pin" => {
                    let code = value_at(&argv, i, "--pin")?;
                    validate_pin(&code)?;
                    args.pin = Some(code);
                    i += 1;
                }
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

/// AP2 配对码是 4~8 位数字（HomeKit 的 PIN 规则）。
fn validate_pin(code: &str) -> Result<()> {
    if !(4..=8).contains(&code.len()) || !code.chars().all(|c| c.is_ascii_digit()) {
        bail!("--pin 要是 4~8 位数字，例如 3939");
    }
    Ok(())
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
      --ap1              只走 AirPlay 1（RAOP 实时流，延迟更低；默认 1 + 2 双支持）
      --transient        无配对码的临时配对（手机端不弹码，但每次连接更慢）
      --pin <配对码>     自定义配对码（默认 {DEFAULT_PIN}，4~8 位数字）
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
        // AP2 的 iOS 会同时开「遥控」与「音频」两条 RTSP 连接；AP1 只放 1 路。
        .max_clients(max_clients(args.ap1))
        // AP2 的多声道源（5.1/7.1）先在库里下混成 2.0，再交给本机声卡。
        .output_max_channels(2)
        // AP1 永远保留；AP2 是否广告由下面的 mode 决定。
        .port(args.port);
    if !args.bind.is_empty() {
        builder = builder.bind(BindConfig::new().addrs(args.bind.clone()));
    }

    // AirPlay 2：默认开 HomeKit 持久配对（配对一次，之后快速重连）。
    // --ap1 退化成纯 RAOP；--transient 换成无配对码的临时配对。
    let mut pairing_hint: Option<String> = None;
    if args.ap1 {
        builder = builder.mode(AirPlayMode::AirPlay1);
    } else {
        builder = builder.mode(AirPlayMode::AirPlay2);
        if !args.transient {
            let pin = args.pin.clone().unwrap_or_else(|| DEFAULT_PIN.to_string());
            builder = builder
                .pin(pin.clone())
                .pairing_store(Arc::new(FilePairingStore::new(pairing_path())));
            pairing_hint = Some(format!(
                "配对码：{pin}（首次连接时在手机上输入一次；配对信息保存在 {}）",
                pairing_path().display()
            ));
        }
    }

    let mut server = builder.build(handler).context("创建 AirPlay 服务失败")?;
    server.start().await.context("AirPlay 服务启动失败")?;

    // mDNS 注册失败时 start() 可能返回 Ok 但服务其实没在跑，以 is_running 为准。
    if !server.is_running() {
        bail!("AirPlay 服务没能启动（端口被占用，或 mDNS 注册失败）");
    }

    let info = server.service_info();
    let mode = if args.ap1 {
        "AirPlay 1（RAOP）"
    } else {
        "AirPlay 1 + AirPlay 2"
    };
    println!();
    println!("✅ {mode} 接收端已就绪：「{}」", info.airplay_name);
    println!("   RTSP 端口：{}", info.port);
    if let Some(ip) = local_ipv4() {
        println!("   本机局域网 IP：{ip}");
    }
    if let Some(hint) = &pairing_hint {
        println!("   🔐 {hint}");
    } else if !args.ap1 {
        println!("   🔐 临时配对（无配对码）：手机端不弹码，但每次连接会慢一些");
    }
    if !args.ap1 {
        println!("   ⓘ AP2 走 UDP 319/320（PTP 时钟）与加密 RTSP；连不上先放行 Windows 防火墙");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_DIR_ID: AtomicUsize = AtomicUsize::new(0);

    /// 每个测试用独立临时目录，避免并行跑时互相看到对方的配对文件。
    fn temp_pairing_path() -> (std::path::PathBuf, std::path::PathBuf) {
        let id = TEMP_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "stereolink-airplay-test-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (dir.clone(), dir.join("airplay-pairings.json"))
    }

    #[test]
    fn pin_must_be_4_to_8_digits() {
        for ok in ["3939", "0000", "12345678"] {
            if let Err(e) = validate_pin(ok) {
                panic!("{ok} 应该合法：{e}");
            }
        }
        for bad in ["", "123", "123456789", "39a9", "3939 ", "３９３９"] {
            assert!(validate_pin(bad).is_err(), "{bad:?} 应该被拒绝");
        }
    }

    #[test]
    fn pairing_store_round_trips_through_disk() {
        let (dir, path) = temp_pairing_path();
        {
            let store = FilePairingStore::new(path.clone());
            assert!(!store.has_any_pairing());
            assert_eq!(store.get("phone"), None);
            store.put("phone", [0x11; 32]);
            store.save_identity([0x22; 32]);
            assert!(store.has_any_pairing());
            assert!(path.is_file(), "配对信息应该立刻落盘");
        }
        // 重新 new 一次 = 模拟进程重启：配对公钥与身份种子都要从文件读回。
        {
            let store = FilePairingStore::new(path.clone());
            assert_eq!(store.get("phone"), Some([0x11; 32]));
            assert_eq!(store.load_identity(), Some([0x22; 32]));
            store.remove("phone");
            assert!(!store.has_any_pairing());
            assert_eq!(store.get("phone"), None);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn defaults_are_dual_mode_with_persistent_pairing() {
        let args = Args::default();
        assert!(!args.ap1, "默认应同时支持 AirPlay 1 + AirPlay 2");
        assert!(!args.transient, "默认走持久配对，第二次连接更快");
        assert_eq!(args.pin, None, "默认配对码在运行时取 DEFAULT_PIN");
        assert_eq!(DEFAULT_PIN, "3939");
        assert_eq!(max_clients(false), MAX_CLIENTS_AP2, "AP2 要能同时接遥控 + 音频两条连接");
        assert_eq!(max_clients(true), 1, "AP1 只放一条实时流，避免两台手机混播");
    }
}
