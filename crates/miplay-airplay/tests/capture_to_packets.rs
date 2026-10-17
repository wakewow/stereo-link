//! 设备无关的集成回归：**采集块 → 增益 → 分块 → RTP 包**。
//!
//! 这条路径就是 `miplay-bridge stream` 真正跑的路径（`miplay-server/src/stream.rs` 里的
//! `Command::Pcm` 分支）：
//!
//! ```text
//! WASAPI 回环 → 10 ms 块 → apply_gain_s16le → PacketChunker → AudioPacketizer::packet → UDP
//! ```
//!
//! 这里不连音箱、不发 UDP（真机验收见 `docs/end-to-end.md`），只对**字节**做断言，
//! 让「改了采集块大小/增益/队列上限就把音频喂坏」这类回归在 CI 里立刻红。
//!
//! 为什么用 `cidermill_airplay` 直接打真实包：`AudioPacketizer` 就是线上用的那个实现，
//! 断言真实头部字节比断言我们自己的封装更有意义（同一套断言已在 `tests/rtp_layout.rs`
//! 里对着 wire 格式校准过）。

use cidermill_airplay::RtpTimestamp;
use cidermill_airplay::audio::rtp::{AUDIO_PAYLOAD_TYPE, HEADER_LEN, TRAILER_LEN, VERSION_BYTE, open_packet};
use cidermill_airplay::audio::{AudioPacketizer, StreamFormat, StreamSetup, StreamType};
use miplay_airplay::{PcmFormat, PacketChunker, apply_gain_s16le};

const KEY: [u8; 32] = [0x5A; 32];
const SSRC: u32 = 0x0BAD_F00D;
const FIRST_SEQ: u32 = 7;
const FIRST_TS: u32 = 44_100;
const FRAMES_PER_PACKET: usize = 352;
/// 8 ms @44.1 kHz：刚好一个包，用它当「采集块」最省事。
const PACKET_BYTES: usize = FRAMES_PER_PACKET * 4;
/// 10 ms 的采集块（`stream.rs` 的默认 `--chunk-ms 10`）。
const TEN_MS_BYTES: usize = 4410 * 4;

fn packetizer() -> AudioPacketizer {
    let setup = StreamSetup::new(
        StreamType::Realtime,
        StreamFormat::PCM_44100_16_STEREO,
        6001,
        KEY,
        0x1234_5678,
    );
    AudioPacketizer::new(&setup, SSRC, FIRST_SEQ, RtpTimestamp::new(FIRST_TS)).expect("打包器")
}

fn new_chunker() -> PacketChunker {
    PacketChunker::new(PcmFormat::S16LE_44100_STEREO, FRAMES_PER_PACKET as u32)
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// 造一段小端 16 bit 交错 PCM：偶声道 = `value`，奇声道 = `-value`。
fn capture_block(frames: usize, value: i16) -> Vec<u8> {
    let mut out = Vec::with_capacity(frames * 4);
    for _ in 0..frames {
        out.extend_from_slice(&value.to_le_bytes());
        out.extend_from_slice(&(-value).to_le_bytes());
    }
    out
}

#[test]
fn eight_ms_capture_chunks_become_one_realtime_packet_each() {
    let mut chunker = new_chunker();
    let mut packetizer = packetizer();
    let mut packets = 0usize;

    for _ in 0..20 {
        chunker.push(&capture_block(FRAMES_PER_PACKET, 1000));
        while let Some(samples) = chunker.take_packet() {
            let packet = packetizer.packet(&samples).expect("打包");
            assert_eq!(
                packet.len(),
                HEADER_LEN + PACKET_BYTES + TRAILER_LEN,
                "实时 LPCM 包长 = 头 12 + 裸 PCM 1408 + 尾 24"
            );
            if packets == 0 {
                assert_eq!(packet[0], VERSION_BYTE);
                assert_eq!(packet[1] & 0x7F, AUDIO_PAYLOAD_TYPE);
                assert_eq!(be16(&packet[2..4]), FIRST_SEQ as u16);
                assert_eq!(be32(&packet[4..8]), FIRST_TS);
                assert_eq!(be32(&packet[8..12]), SSRC);
                let opened = open_packet(&KEY, &packet).expect("同一个 shk 应当能解开");
                assert_eq!(opened.payload.len(), PACKET_BYTES);
                // 线上是大端：1000 = 0x03E8，-1000 = 0xFC18
                assert_eq!(&opened.payload[..4], &[0x03, 0xE8, 0xFC, 0x18]);
            }
            packets += 1;
        }
    }

    assert_eq!(packets, 20, "20 个 8 ms 块 = 20 个包，一帧不多一帧不少");
    assert_eq!(packetizer.packets_sent(), 20);
    assert_eq!(chunker.queued_bytes(), 0, "刚好整除，没有余量");
    assert_eq!(chunker.dropped_bytes(), 0);
}

#[test]
fn ten_ms_chunks_are_split_into_packets_without_losing_a_frame() {
    let mut chunker = new_chunker();
    let mut packetizer = packetizer();
    let mut frames = 0usize;

    // 10 个 10 ms 块 = 100 ms = 4410 帧 = 12 个整包（4224 帧）+ 186 帧余量
    for _ in 0..10 {
        chunker.push(&capture_block(441, 2000));
        while let Some(samples) = chunker.take_packet() {
            assert_eq!(samples.len(), FRAMES_PER_PACKET * 2);
            frames += FRAMES_PER_PACKET;
            packetizer.packet(&samples).expect("打包");
        }
    }

    assert_eq!(packetizer.packets_sent(), 12, "4410 帧 / 352 = 12.53 → 12 个整包");
    assert_eq!(frames, 12 * FRAMES_PER_PACKET);
    assert_eq!(chunker.queued_bytes(), 186 * 4, "剩下的 186 帧还在队列里，没丢");
    assert_eq!(chunker.dropped_bytes(), 0);

    // 收尾时不足一包的部分靠补静音凑齐，并**明确标出来**
    let (samples, padded) = chunker.take_packet_padded().expect("补静音包");
    assert!(padded, "这一包是欠载补出来的");
    assert_eq!(samples.len(), FRAMES_PER_PACKET * 2);
    assert_eq!(packetizer.packets_sent(), 12, "还没发就不算已发");
    packetizer.packet(&samples).expect("打包");
    assert_eq!(packetizer.packets_sent(), 13, "补静音的包也要发，时间线才不断");
    assert_eq!(chunker.queued_bytes(), 0);
}

#[test]
fn a_partial_packet_is_padded_and_flagged() {
    let mut chunker = new_chunker();
    chunker.push(&capture_block(88, 500)); // 2 ms，远不够一包

    assert!(chunker.take_packet().is_none(), "不足一包时 take_packet 给 None");
    let (samples, padded) = chunker.take_packet_padded().expect("补静音后应当有一包");
    assert!(padded, "必须标出这一包是补出来的（= 欠载）");
    assert_eq!(samples.len(), FRAMES_PER_PACKET * 2);
    // 前 88 帧是真音频，其余是 0
    assert_eq!(samples[0], 500);
    assert_eq!(samples[87 * 2], 500);
    assert_eq!(samples[88 * 2], 0);
    assert!(samples[FRAMES_PER_PACKET * 2 - 1] == 0);
    assert!(chunker.take_packet_padded().is_none(), "补完就空了");
}

#[test]
fn padding_keeps_the_rtp_timeline_moving() {
    let mut chunker = new_chunker();
    let mut packetizer = packetizer();

    chunker.push(&capture_block(88, 300));
    let (padded_samples, padded) = chunker.take_packet_padded().expect("补静音包");
    assert!(padded);
    let first = packetizer.packet(&padded_samples).expect("第一包");

    chunker.push(&capture_block(FRAMES_PER_PACKET, 300));
    let (real_samples, padded) = chunker.take_packet_padded().expect("整包");
    assert!(!padded);
    let second = packetizer.packet(&real_samples).expect("第二包");

    assert_eq!(
        be32(&second[4..8]),
        be32(&first[4..8]) + FRAMES_PER_PACKET as u32,
        "补静音也要推进时间戳，否则接收端的时间线会错位"
    );
    assert_eq!(be16(&second[2..4]), be16(&first[2..4]).wrapping_add(1));
}

#[test]
fn gain_is_applied_before_packetizing_and_saturates_instead_of_wrapping() {
    let mut block = capture_block(FRAMES_PER_PACKET, 30_000);

    apply_gain_s16le(&mut block, 4.0).expect("增益合法");
    let mut chunker = new_chunker();
    chunker.push(&block);
    let samples = chunker.take_packet().expect("一包");
    assert!(
        samples.iter().all(|s| *s == i16::MAX as i32 || *s == i16::MIN as i32),
        "30000 × 4 必须饱和到 ±满量程，不能回绕（回绕会变成刺耳的爆音）"
    );

    let mut quiet = capture_block(FRAMES_PER_PACKET, 1000);
    apply_gain_s16le(&mut quiet, 0.5).expect("增益合法");
    let mut chunker = new_chunker();
    chunker.push(&quiet);
    let samples = chunker.take_packet().expect("一包");
    assert_eq!(samples[0], 500);
    assert_eq!(samples[1], -500);
}

#[test]
fn gain_rejects_absurd_values_before_touching_audio() {
    let mut block = capture_block(4, 100);
    for bad in [f32::NAN, -0.1, 9.0] {
        assert!(apply_gain_s16le(&mut block, bad).is_err(), "gain={bad} 必须被拒");
    }
    let mut zero = capture_block(4, 777);
    apply_gain_s16le(&mut zero, 0.0).expect("0 是合法的静音增益");
    assert!(zero.iter().all(|b| *b == 0));
}

#[test]
fn queued_audio_is_bounded_by_trim_to() {
    let mut chunker = new_chunker();
    // 喂 1 秒，然后按 MAX_QUEUED_MS=40 ms 的策略裁掉多余的
    for _ in 0..100 {
        chunker.push(&capture_block(441, 100));
    }
    assert_eq!(chunker.queued_ms(), 1000.0);

    let limit_bytes = PcmFormat::S16LE_44100_STEREO.bytes_for_frames(44100 * 40 / 1000);
    let dropped = chunker.trim_to(limit_bytes);

    assert_eq!(dropped, 44_100 * 4 - limit_bytes, "被裁掉的正好是超出的字节");
    assert_eq!(chunker.dropped_bytes(), dropped as u64);
    assert_eq!(chunker.queued_ms(), 40.0, "排队的音频被压到 40 ms");
}
