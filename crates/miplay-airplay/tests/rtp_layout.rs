//! 无需真机即可验证的**线上格式**测试：RTP 音频包头字节布局、ChaCha20-Poly1305 封装/nonce 约定、
//! 同步包（声明延迟）布局。
//!
//! 这些断言的价值：真机调试时抓到的包能立刻对上号；库升级后布局漂移会在这里先炸，而不是在音箱上。
//! 期望值来源：`research/airplay/protocol-notes.md`（协议笔记，含真机抓包与 pyatv 交叉验证）。

use cidermill_airplay::audio::rtp::{
    open_packet, AUDIO_PAYLOAD_TYPE, HEADER_LEN, NONCE_LEN, SYNC_FLAGS, SYNC_PACKET_LEN, TAG_LEN,
    TRAILER_LEN, VERSION_BYTE,
};
use cidermill_airplay::audio::{AudioPacketizer, StreamFormat, StreamSetup, StreamType};
use cidermill_airplay::{NtpTimestamp, RtpTimestamp};

/// 测试用共享密钥（真机上来自 HAP 配对派生的 `shk`，这里用固定值即可）。
const KEY: [u8; 32] = [0x42; 32];
const SSRC: u32 = 0x1122_3344;
const FIRST_SEQ: u32 = 1000;
const FIRST_TS: u32 = 1_000_000;
const FRAMES_PER_PACKET: usize = 352;
const SAMPLES_PER_PACKET: usize = FRAMES_PER_PACKET * 2; // 立体声交错

fn realtime(format: StreamFormat) -> AudioPacketizer {
    let setup = StreamSetup::new(StreamType::Realtime, format, 6001, KEY, 0xABCD_EF01);
    AudioPacketizer::new(&setup, SSRC, FIRST_SEQ, RtpTimestamp::new(FIRST_TS))
        .expect("构造包化器")
}

fn be16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[test]
fn lpcm_packet_header_matches_the_wire_format() {
    let mut packetizer = realtime(StreamFormat::PCM_44100_16_STEREO);
    let packet = packetizer
        .packet(&vec![0i32; SAMPLES_PER_PACKET])
        .expect("打包");

    // 12 字节 RTP 头 + 负载（352 帧 × 2 声道 × 2 字节）+ 16 字节 tag + 8 字节 nonce
    assert_eq!(
        packet.len(),
        HEADER_LEN + FRAMES_PER_PACKET * 4 + TRAILER_LEN,
        "实时 LPCM 包长 = 头 12 + 裸 PCM 1408 + 尾 24"
    );
    assert_eq!(packet[0], VERSION_BYTE, "第一个字节固定 0x80（RTP v2）");
    assert_eq!(
        packet[1] & 0x7F,
        AUDIO_PAYLOAD_TYPE,
        "负载类型 0x60（低 7 位），最高位是 marker"
    );
    assert_eq!(be16(&packet[2..4]), FIRST_SEQ as u16, "序列号");
    assert_eq!(be32(&packet[4..8]), FIRST_TS, "时间戳");
    assert_eq!(be32(&packet[8..12]), SSRC, "SSRC");
}

#[test]
fn realtime_sequence_and_timestamp_step_once_per_packet() {
    let mut packetizer = realtime(StreamFormat::PCM_44100_16_STEREO);
    let first = packetizer
        .packet(&vec![0i32; SAMPLES_PER_PACKET])
        .expect("第一包");
    let second = packetizer
        .packet(&vec![0i32; SAMPLES_PER_PACKET])
        .expect("第二包");

    assert_eq!(be16(&second[2..4]), be16(&first[2..4]).wrapping_add(1));
    assert_eq!(be32(&second[4..8]), be32(&first[4..8]) + 352);
    assert_eq!(packetizer.packets_sent(), 2);
}

#[test]
fn payload_is_big_endian_lpcm_behind_chacha20_poly1305() {
    let mut packetizer = realtime(StreamFormat::PCM_44100_16_STEREO);
    let mut samples = Vec::with_capacity(SAMPLES_PER_PACKET);
    for i in 0..SAMPLES_PER_PACKET {
        samples.push(if i % 2 == 0 { 1000 } else { -1000 });
    }
    let packet = packetizer.packet(&samples).expect("打包");

    let opened = open_packet(&KEY, &packet).expect("用同一个 shk 应当能解开");
    assert_eq!(opened.payload.len(), FRAMES_PER_PACKET * 4);
    // LPCM 在线上是大端 16 bit：1000 = 0x03E8，-1000 = 0xFC18
    assert_eq!(&opened.payload[..4], &[0x03, 0xE8, 0xFC, 0x18]);

    // 尾部 24 字节：Poly1305 tag(16) + nonce(8)
    assert_eq!(packet.len(), HEADER_LEN + opened.payload.len() + TAG_LEN + NONCE_LEN);
}

#[test]
fn wrong_key_fails_authentication() {
    let mut packetizer = realtime(StreamFormat::PCM_44100_16_STEREO);
    let packet = packetizer
        .packet(&vec![1234i32; SAMPLES_PER_PACKET])
        .expect("打包");

    let mut other_key = KEY;
    other_key[0] ^= 0x01;
    assert!(
        open_packet(&other_key, &packet).is_err(),
        "密钥不对必须解密失败（Poly1305 认证），不能悄悄出噪声"
    );
}

#[test]
fn consecutive_packets_never_reuse_a_nonce() {
    let mut packetizer = realtime(StreamFormat::PCM_44100_16_STEREO);
    let first = packetizer
        .packet(&vec![0i32; SAMPLES_PER_PACKET])
        .expect("第一包");
    let second = packetizer
        .packet(&vec![0i32; SAMPLES_PER_PACKET])
        .expect("第二包");

    let nonce1 = &first[first.len() - NONCE_LEN..];
    let nonce2 = &second[second.len() - NONCE_LEN..];
    assert_ne!(nonce1, nonce2, "同一密钥下 nonce 复用会毁掉 ChaCha20-Poly1305");
}

#[test]
fn alac_framing_matches_pcm_and_does_not_save_bandwidth() {
    let mut packetizer = realtime(StreamFormat::ALAC_44100_16_STEREO);
    let packet = packetizer
        .packet(&vec![0i32; SAMPLES_PER_PACKET])
        .expect("打包");

    assert_eq!(packet[0], VERSION_BYTE);
    assert_eq!(packet[1] & 0x7F, AUDIO_PAYLOAD_TYPE);
    assert_eq!(be16(&packet[2..4]), FIRST_SEQ as u16);

    let opened = open_packet(&KEY, &packet).expect("解开 ALAC 包");
    assert!(!opened.payload.is_empty(), "ALAC 负载不能为空");
    // 实测事实：`alac` crate 编的是「逐字（verbatim）」帧，静音也不压缩，
    // 负载 = 裸 PCM 1408 字节 + 4 字节帧头 = 1412 字节，比 L16 还大 4 字节。
    // 所以选 ALAC 不是为了省带宽（两者都是 ~1.4 Mbit/s），只是与 pyatv 实测路径一致。
    let raw = FRAMES_PER_PACKET * 4;
    assert!(
        (raw..=raw + 8).contains(&opened.payload.len()),
        "ALAC 负载实测 {} 字节，应落在裸 PCM {raw}~{raw}+8（帧头）之间",
        opened.payload.len()
    );
}

#[test]
fn sync_packet_declares_exactly_the_latency_we_ask_for() {
    let packetizer = realtime(StreamFormat::PCM_44100_16_STEREO);
    let ntp = NtpTimestamp::now();
    let sync = packetizer.sync_packet(true, 11_025, ntp);

    assert_eq!(sync.flags, SYNC_FLAGS);
    assert_eq!(sync.flags, 7, "当前发送端用 7（旧 AirPlay 用 4）");
    assert_eq!(
        sync.now.get().wrapping_sub(sync.now_without_latency.get()),
        11_025,
        "now - now_without_latency == 我们声明的延迟帧数"
    );

    let bytes = sync.encode();
    assert_eq!(bytes.len(), SYNC_PACKET_LEN);
    assert_eq!(bytes[0], 0x90, "第一个同步包置 0x90");
    assert_eq!(bytes[1], 0xD4, "0x80 | 0x54 = 同步包类型");
    assert_eq!(be16(&bytes[2..4]), 7);
    assert_eq!(be32(&bytes[4..8]), sync.now_without_latency.get());
    assert_eq!(be32(&bytes[16..20]), sync.now.get());
}

#[test]
fn later_sync_packets_clear_the_first_bit() {
    let packetizer = realtime(StreamFormat::PCM_44100_16_STEREO);
    let ntp = NtpTimestamp::now();
    assert_eq!(packetizer.sync_packet(true, 11_025, ntp).encode()[0], 0x90);
    assert_eq!(packetizer.sync_packet(false, 11_025, ntp).encode()[0], 0x80);
}

#[test]
fn lpcm_packet_is_padded_to_a_whole_packet() {
    // 采集端给半包数据时，包化器必须补到整包帧数（时间轴不能断）
    let mut packetizer = realtime(StreamFormat::PCM_44100_16_STEREO);
    let packet = packetizer.packet(&[7i32; 4]).expect("打包");
    let opened = open_packet(&KEY, &packet).expect("解开");
    assert_eq!(opened.payload.len(), FRAMES_PER_PACKET * 4);
    assert_eq!(&opened.payload[..8], &[0, 7, 0, 7, 0, 7, 0, 7]);
    assert_eq!(&opened.payload[8..16], &[0u8; 8], "缺的部分补零");
}
