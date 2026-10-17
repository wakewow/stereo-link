//! 真机探针：找出「设备收下 `StreamSetup` 却不回包」的字段级原因。
//!
//! 实测时间线（`example tone`）：
//! `GET /info 200 -> 配对 200 -> 会话 SETUP 8ms 内 200 -> 发出 StreamSetup 后读超时(10060)`。
//! 而 pyatv 在**同一台设备**上成功，且 `StreamSetup` body 与我们的逐字段相同
//! （`tests/wire_requests.rs` 已断言），SETUP#1 的字段集补成 pyatv 的形状后仍然卡住
//! （本文件 V1，见 `research/airplay/protocol-notes.md`）。
//!
//! 剩下唯一成体系的差别是**配对形状**：
//! * pyatv 先 `POST /pair-pin-start`（`X-Apple-HKP: 4`），再两次 `POST /pair-setup`，**从不做 pair-verify**；
//! * cidermill 的 `Authenticate::Transient(TransientStyle::Ios26)` 会先发**两个被放弃的
//!   `/pair-verify` M1**（`X-Apple-HKP: 6`、`3`，见 `sequence::IOS_PAIR_VERIFY_HKP_LADDER`），
//!   而且 `authenticate()` **不调** `hap_pin_start()`。
//!
//! 于是本探针把配对维度做成矩阵，其它一切（SETUP#1 字段集 = pyatv 全集、编解码 = LPCM）保持不变：
//! * V0 `Ios26` + 不先 pin-start ＝ 现在 `authenticate()` 的路径（已知卡住）
//! * V1 `Ios26` + 先 pin-start
//! * V2 `Classic` + 先 pin-start ＝ pyatv 的形状
//! * V3 `Ios26WithoutVerifyProbes` + 先 pin-start
//!
//! 只建会话、**不发一个音频包、不改音量**，每个变体结束都 `TEARDOWN`。
//! 用法：`cargo run -p miplay-airplay --offline --example setup_probe -- [ip] [only] [pause] [retries]`
use std::io::Cursor;
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cidermill_airplay::audio::{
    Action, AudioSendConfig, AudioSendState, StreamFormat, StreamType, TimingMode,
};
use cidermill_airplay::pairing::TransientStyle;
use cidermill_airplay::session::{self, SetupParams};
use cidermill_airplay::timing::TimingServer;
use cidermill_airplay::util;
use cidermill_airplay::{ControlChannel, NtpTimestamp, OsRng};

/// 一个配对形状变体；SETUP#1 的字段集在所有变体里都固定为 pyatv 的全集。
struct Variant {
    label: &'static str,
    style: TransientStyle,
    /// 是否先发 `POST /pair-pin-start`（`X-Apple-HKP: 4`），pyatv 会发，cidermill 不发。
    pin_start: bool,
}

const VARIANTS: [Variant; 4] = [
    Variant {
        label: "V0 Ios26，不先 /pair-pin-start（＝ authenticate() 现路径）",
        style: TransientStyle::Ios26,
        pin_start: false,
    },
    Variant {
        label: "V1 Ios26 + 先 /pair-pin-start",
        style: TransientStyle::Ios26,
        pin_start: true,
    },
    Variant {
        label: "V2 Classic（无 pair-verify 探针）+ 先 /pair-pin-start（＝ pyatv 形状）",
        style: TransientStyle::Classic,
        pin_start: true,
    },
    Variant {
        label: "V3 Ios26WithoutVerifyProbes + 先 /pair-pin-start",
        style: TransientStyle::Ios26WithoutVerifyProbes,
        pin_start: true,
    },
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let host = args.next().unwrap_or_else(|| "192.168.1.21".to_string());
    let only: Option<usize> = args.next().and_then(|s| s.parse().ok());
    let pause: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(6);
    println!("配对形状探针 -> {host}:7000（不发音频、不改音量；变体间隔 {pause}s）");
    for (index, variant) in VARIANTS.iter().enumerate() {
        if let Some(only) = only {
            if index != only {
                continue;
            }
        }
        if index > 0 {
            println!("  （等 {pause}s 让设备收尾上一个会话）");
            std::thread::sleep(Duration::from_secs(pause));
        }
        println!("\n=== V{index} {} ===", variant.label);
        if let Err(err) = attempt(&host, variant) {
            println!("  变体中断：{err}");
        }
    }
    println!("\n完成：每个变体各自独立连接，均已 TEARDOWN。");
    Ok(())
}

/// 打印一个请求的头部（只挑与参照序列有关的那几个）。
fn headers_line(headers: &cidermill_airplay::rtsp::Headers) -> String {
    const KEYS: [&str; 7] = [
        "CSeq",
        "User-Agent",
        "Content-Type",
        "Content-Length",
        "Active-Remote",
        "DACP-ID",
        "Client-Instance",
    ];
    KEYS.iter()
        .filter_map(|key| headers.get(key).map(|value| format!("{key}: {value}")))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// 连接 + GET /info + 配对；失败重试（设备在被打断的会话后会短暂拒绝配对）。
fn connect_and_pair(
    host: &str,
    variant: &Variant,
    retries: u32,
) -> Result<(ControlChannel, Vec<String>), Box<dyn std::error::Error>> {
    let mut last: Option<String> = None;
    for attempt in 0..retries.max(1) {
        if attempt > 0 {
            println!("  第 {} 次重试（等 6s 让设备恢复）", attempt + 1);
            std::thread::sleep(Duration::from_secs(6));
        }
        let started = Instant::now();
        let mut channel = match ControlChannel::builder(host)
            .port(7000)
            .timeout(Duration::from_secs(3))
            .connect()
        {
            Ok(channel) => channel,
            Err(err) => {
                last = Some(format!("连接失败：{err}"));
                continue;
            }
        };
        if let Err(err) = channel.device_info() {
            last = Some(format!("GET /info 失败：{err}"));
            continue;
        }
        if variant.pin_start {
            match channel.hap_pin_start() {
                Ok(()) => println!("  /pair-pin-start -> 200"),
                Err(err) => {
                    last = Some(format!("/pair-pin-start 失败：{err}"));
                    continue;
                }
            }
        }
        let mut trace = Vec::new();
        match channel.pair_setup_transient_traced(variant.style, &mut OsRng, &mut trace) {
            Ok(keys) => {
                for line in &trace {
                    println!("    {line}");
                }
                println!(
                    "  配对完成（{} ms，style={:?}，pin_start={}）",
                    started.elapsed().as_millis(),
                    variant.style,
                    variant.pin_start
                );
                channel.enable_encryption(&keys)?;
                println!(
                    "  控制通道已加密：{}，会话密钥存在：{}",
                    channel.is_encrypted(),
                    channel.session_keys().is_some()
                );
                return Ok((channel, trace));
            }
            Err(err) => {
                for line in &trace {
                    println!("    {line}");
                }
                last = Some(format!("配对失败：{err}"));
            }
        }
    }
    Err(format!("重试耗尽：{}", last.unwrap_or_default()).into())
}

fn attempt(host: &str, variant: &Variant) -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let retries: u32 = std::env::args()
        .nth(4)
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    println!("  [连接 + GET /info + 配对（style={:?}, pin_start={}）]", variant.style, variant.pin_start);
    let (mut channel, _trace) = connect_and_pair(host, variant, retries)?;
    let local = channel.connection().local_ip();

    // 设备会主动往这个口打时序探测，必须真有一个服务器在应答
    let timing = TimingServer::start_on(local, Arc::new(NtpTimestamp::now))?;

    let mut params = SetupParams::new(
        util::random_mac(&mut OsRng),
        util::uuid_v4(&mut OsRng),
        "miplay-bridge".to_string(),
    );
    params.timing_port = Some(timing.port());
    // 字段集固定为 pyatv 的全集（上一轮已证明它不是原因，本探针只变配对形状）
    params.is_multi_select_airplay = Some(true);
    params.stats_collection_enabled = Some(false);

    let body = pyatv_extra_body(&params)?;
    println!("  SETUP#1 字段：{}", describe_body(&body)?);

    let mut request = channel.connection().setup_request(&params)?;
    request.headers.set("Content-Length", body.len().to_string());
    request.body = body;
    println!(
        "  SETUP#1 URI {}（{} 字节）",
        request.uri,
        request.body.len()
    );
    println!("    头部：{}", headers_line(&request.headers));

    let reply = match channel.request(request) {
        Ok(reply) => reply,
        Err(err) => {
            println!("  SETUP#1 失败：{err}");
            return Ok(());
        }
    };
    println!("  SETUP#1 -> {} {}", reply.status, reply.reason);
    if !reply.is_success() {
        return Ok(());
    }
    let setup = session::SetupResponse::parse(&reply.body)?;
    println!("  eventPort={} ", setup.event_port);
    let _events = if setup.event_port == 0 {
        None
    } else {
        Some(channel.open_event_channel(setup.event_port)?)
    };

    // 实时 LPCM：与 pyatv 成功那次用的编解码一致（audioFormat 2048 / ct 1）
    let data = UdpSocket::bind((local, 0))?;
    let control = UdpSocket::bind((local, 0))?;
    let config = AudioSendConfig::generate(
        channel.connection(),
        StreamType::Realtime,
        StreamFormat::PCM_44100_16_STEREO,
        control.local_addr()?.port(),
        TimingMode::Ntp,
        &mut OsRng,
    );
    let mut state = AudioSendState::new(config);
    let _packetizer = state.packetizer()?;

    let clock = Instant::now();
    for _ in 0..8 {
        match state.poll(clock.elapsed()) {
            Action::Send { step, request } => {
                let name = format!("{step:?}");
                let is_stream_setup = name.contains("StreamSetup");
                let request = request;
                println!(
                    "  发出 {name}（URI {}，{} 字节）",
                    request.uri,
                    request.body.len()
                );
                println!("    头部：{}", headers_line(&request.headers));
                if is_stream_setup {
                    println!("    body：{}", describe_body(&request.body)?);
                }
                let at = Instant::now();
                match channel.session_request(request) {
                    Ok(reply) => {
                        println!(
                            "    -> {} {}（{} ms）",
                            reply.status,
                            reply.reason,
                            at.elapsed().as_millis()
                        );
                        state.on_response(&reply)?;
                        if is_stream_setup {
                            break;
                        }
                    }
                    Err(err) => {
                        println!("    !! 无响应：{err}（{} ms）", at.elapsed().as_millis());
                        break;
                    }
                }
            }
            Action::ConnectData {
                data_port,
                control_port,
                ..
            } => {
                println!("  连接数据口 {data_port} / 控制口 {control_port}");
                data.connect((host, data_port))?;
                control.connect((host, control_port))?;
                state.data_connected();
            }
            Action::StartStreaming { .. } => {
                println!("  已进入推流状态");
                break;
            }
            Action::Failed {
                step,
                status,
                reason,
            } => {
                println!("  {step:?} 失败：{status:?} {reason}");
                break;
            }
            Action::Finished => break,
            _ => {}
        }
    }

    let _ = channel.teardown();
    println!("  用时 {} ms，已 TEARDOWN", started.elapsed().as_millis());
    Ok(())
}

/// cidermill 的 `SetupParams` 没有 `groupContainsGroupLeader` / `senderSupportsRelay` 两个字段，
/// 这里把它的 body 解成 plist、补上两个键再序列化回去，凑出 pyatv 的完整字段集。
fn pyatv_extra_body(params: &SetupParams) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let base = session::setup_body(params)?;
    let value = plist::Value::from_reader(Cursor::new(&base))?;
    let mut dict = value.into_dictionary().ok_or("SETUP body 不是字典")?;
    dict.insert(
        "groupContainsGroupLeader".to_string(),
        plist::Value::Boolean(false),
    );
    dict.insert(
        "senderSupportsRelay".to_string(),
        plist::Value::Boolean(false),
    );
    let mut out = Vec::new();
    plist::to_writer_binary(&mut out, &plist::Value::Dictionary(dict))?;
    Ok(out)
}

/// 把二进制 plist body 打成一行的 `键=值` 列表（键排序，`Data` 只报长度）。
fn describe_body(body: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
    let value = plist::Value::from_reader(Cursor::new(body))?;
    let dict = value.as_dictionary().ok_or("body 不是字典")?;
    let mut parts: Vec<String> = dict
        .iter()
        .map(|(key, value)| format!("{key}={}", short(value)))
        .collect();
    parts.sort();
    Ok(parts.join(" "))
}

fn short(value: &plist::Value) -> String {
    match value {
        plist::Value::Data(bytes) => format!("<{} 字节>", bytes.len()),
        other => format!("{other:?}"),
    }
}
