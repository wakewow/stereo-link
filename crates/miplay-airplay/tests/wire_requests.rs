//! 请求线格式的离线核对——不碰网络。
//!
//! 起因：我们的发送端连上音箱后**卡在第二个 RTSP 请求上**（请求已写出，10 秒读超时，
//! 原始错误 `os error 10060`），而 pyatv 0.18.0 对**同一台**音箱（Xiaomi Sound-左 L16A，
//! `Server: AirTunes/366.0`）推流是通的。所以第一步不是猜，而是把我们状态机要发的请求
//! 原样 dump 出来，与已知可用客户端的字节级记录逐项对照。
//!
//! 参照数据：`research/airplay/reference_sequence.json`，由
//! `research/airplay/extract_reference_sequence.py` 从 pyatv 的 DEBUG 日志还原，
//! 含每个请求的完整 body（二进制 plist 已解码）。
//!
//! 这些测试只做**结构**断言（字段存在、量纲正确、我们自己传入的值被原样带上）；
//! 与 pyatv 的取值差异打印出来供人判断，不断言成败——两者都是协议允许的写法。

use std::time::Duration;

use cidermill_airplay::audio::{
    Action, AudioSendConfig, AudioSendState, Step, StreamFormat, StreamType, TimingMode,
};
use cidermill_airplay::{OsRng, SenderConnection};

/// pyatv 在本机音箱上成功用过的 `SETUP`(streams) 取值，见 reference_sequence.json 第 6 条。
const PYATV_DEVICE: &str = "192.168.1.21";
const PYATV_CONTROL_PORT: u64 = 57_726;
const PYATV_AUDIO_FORMAT: u64 = 2_048; // 1 << 11 = PCM 44.1k/16bit/立体声
const PYATV_CT: u64 = 1; // compression = LPCM
const PYATV_SPF: u64 = 352;
const PYATV_SR: u64 = 44_100;
const PYATV_TYPE: u64 = 96;
const PYATV_LATENCY_MIN: u64 = 11_025;
const PYATV_LATENCY_MAX: u64 = 88_200;

/// 本地地址用实测的这台机器；`generate` 里没有 I/O。
const LOCAL_IP: &str = "192.168.1.4";

/// 跑一次状态机，取回它要发的**第一个**请求（对我们来说就是流 `SETUP`）。
fn first_request(format: StreamFormat) -> (Step, String, Vec<u8>) {
    let mut rng = OsRng;
    let local: std::net::IpAddr = LOCAL_IP.parse().unwrap();
    let connection = SenderConnection::new(PYATV_DEVICE, local, &mut rng);
    let config = AudioSendConfig::generate(
        &connection,
        StreamType::Realtime,
        format,
        PYATV_CONTROL_PORT as u16,
        TimingMode::Ntp,
        &mut rng,
    );
    let mut state = AudioSendState::new(config);
    match state.poll(Duration::ZERO) {
        Action::Send { step, request } => (
            step,
            String::from_utf8_lossy(&request.encode()).into_owned(),
            request.body.clone(),
        ),
        other => panic!("第一个动作应当是 SETUP，实际是 {other:?}"),
    }
}

/// `SETUP` 的请求头：请求行 + Content-Type，必须和 pyatv 一致。
fn assert_setup_head(wire: &str) {
    let head = wire.split("\r\n\r\n").next().unwrap_or_default();
    assert!(
        head.starts_with(&format!("SETUP rtsp://{LOCAL_IP}/")),
        "请求行不对：{head}"
    );
    assert!(
        head.contains("Content-Type: application/x-apple-binary-plist"),
        "SETUP 的 body 必须是二进制 plist：{head}"
    );
}

/// 把 plist 里的 `streams[0]` 取出来，并打印成可对比的一行。
fn streams0(body: &[u8]) -> plist::Dictionary {
    let value = plist::Value::from_reader(std::io::Cursor::new(body))
        .expect("SETUP body 应当是二进制 plist");
    let dict = value.as_dictionary().expect("顶层应当是字典");
    let streams = dict
        .get("streams")
        .and_then(plist::Value::as_array)
        .expect("顶层应当有 streams 数组");
    let first = streams
        .first()
        .and_then(plist::Value::as_dictionary)
        .expect("streams[0] 应当是字典");
    println!(
        "streams[0] = {}",
        first
            .iter()
            .map(|(k, v)| format!(
                "{k}={}",
                match v {
                    plist::Value::Data(d) => format!("<{} 字节>", d.len()),
                    other => format!("{other:?}"),
                }
            ))
            .collect::<Vec<_>>()
            .join(" ")
    );
    first.clone()
}

fn uint(dict: &plist::Dictionary, key: &str) -> u64 {
    dict.get(key)
        .unwrap_or_else(|| panic!("缺字段 {key}"))
        .as_unsigned_integer()
        .unwrap_or_else(|| panic!("{key} 不是无符号整数"))
}

#[test]
fn stream_setup_carries_every_field_the_device_needs() {
    let (step, wire, body) = first_request(StreamFormat::ALAC_44100_16_STEREO);
    assert_eq!(step, Step::StreamSetup);
    assert_setup_head(&wire);

    let s = streams0(&body);
    // 形状：与 pyatv 成功那一次完全一致的键与量纲
    assert_eq!(uint(&s, "type"), PYATV_TYPE, "realtime 音频流是 type=96");
    assert_eq!(uint(&s, "sr"), PYATV_SR);
    assert_eq!(uint(&s, "spf"), PYATV_SPF);
    assert_eq!(uint(&s, "latencyMin"), PYATV_LATENCY_MIN);
    assert_eq!(uint(&s, "latencyMax"), PYATV_LATENCY_MAX);
    assert_eq!(
        uint(&s, "controlPort"),
        PYATV_CONTROL_PORT,
        "必须把我们自己的控制口告诉设备（它按这个口发重传/时序）"
    );
    assert_eq!(
        s.get("isMedia").and_then(plist::Value::as_boolean),
        Some(true)
    );
    assert_eq!(
        s.get("audioMode").and_then(plist::Value::as_string),
        Some("default")
    );
    let shk = s
        .get("shk")
        .and_then(plist::Value::as_data)
        .expect("shk 必须是 Data");
    assert_eq!(shk.len(), 32, "shk 是 ChaCha20-Poly1305 的流密钥");
    // streamConnectionID 应当是本次会话的数字 id（它同时是 realtime 的 SSRC 来源）
    assert!(
        uint(&s, "streamConnectionID") > 0,
        "streamConnectionID 不能是 0"
    );
    println!(
        "ALAC 变体：audioFormat={} ct={}（pyatv 那次是 audioFormat={PYATV_AUDIO_FORMAT} ct={PYATV_CT}）",
        uint(&s, "audioFormat"),
        uint(&s, "ct")
    );
}

#[test]
fn pcm_variant_reproduces_the_known_good_client_exactly() {
    let (step, wire, body) = first_request(StreamFormat::PCM_44100_16_STEREO);
    assert_eq!(step, Step::StreamSetup);
    assert_setup_head(&wire);

    let s = streams0(&body);
    assert_eq!(
        uint(&s, "audioFormat"),
        PYATV_AUDIO_FORMAT,
        "LPCM 44.1k/16bit/立体声 = 1<<11，正是 pyatv 报给这台音箱的值"
    );
    assert_eq!(uint(&s, "ct"), PYATV_CT, "LPCM 的压缩类型是 1");
    assert_eq!(uint(&s, "type"), PYATV_TYPE);
    assert_eq!(uint(&s, "sr"), PYATV_SR);
    assert_eq!(uint(&s, "spf"), PYATV_SPF);
    assert_eq!(uint(&s, "latencyMin"), PYATV_LATENCY_MIN);
    assert_eq!(uint(&s, "latencyMax"), PYATV_LATENCY_MAX);
}

/// ALAC 与 LPCM 两个变体唯一的差别就是 `audioFormat`/`ct`，其余字段一字不差。
/// 这条测试把「换编解码只动这两个字段」钉住，方便真机上一键切换做 A/B。
#[test]
fn alac_and_pcm_differ_only_in_audio_format_and_compression() {
    let (_, _, alac_body) = first_request(StreamFormat::ALAC_44100_16_STEREO);
    let (_, _, pcm_body) = first_request(StreamFormat::PCM_44100_16_STEREO);
    let alac = streams0(&alac_body);
    let pcm = streams0(&pcm_body);

    assert_eq!(alac.len(), pcm.len(), "两个变体的字段数量应当相同");
    for (key, av) in &alac {
        let pv = pcm
            .get(key)
            .unwrap_or_else(|| panic!("LPCM 变体缺字段 {key}"));
        if key == "audioFormat" || key == "ct" || key == "shk" || key == "streamConnectionID" {
            // 前两个是编解码选择；shk 是每次会话新生成的共享密钥；
            // streamConnectionID 来自 SenderConnection 的每次会话随机 id（实测两次运行不同），
            // 三者本来就该随会话变化，不属于「随编解码变化」。
            continue;
        }
        assert_eq!(av, pv, "字段 {key} 不应当随编解码变化");
    }
    assert_eq!(uint(&alac, "audioFormat"), 1 << 18, "ALAC 44.1k/16bit = 1<<18");
    assert_eq!(uint(&alac, "ct"), 2, "ALAC 的压缩类型是 2");
}
