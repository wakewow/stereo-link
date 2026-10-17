//! WAV（RIFF/WAVE）头构造。
//!
//! 分发出去的每一条流都是「无限长」的 WAV：头里的 `data` chunk 大小写死成一个
//! 很大但不会溢出的值（[`FAKE_DATA_SIZE`]），音箱/浏览器读到流结束为止。
//! 这与 Python 参考实现 `mixplay-pc-audio/pc_stereo_hub.py` 完全一致。

use std::io::Write;
use std::path::Path;

/// 假长度：约 2 GiB - 512 B，保证 `36 + data_size` 不溢出 u32。
pub const FAKE_DATA_SIZE: u32 = 0x7FFF_FF00;

/// 标准 PCM WAV 头长度。
pub const WAV_HEADER_LEN: usize = 44;

/// 构造 44 字节标准 PCM WAV 头。
///
/// 布局：`RIFF | riffSize(LE u32) | WAVE | fmt_ | 16 | audioFormat=1 | channels |
/// samplerate | byteRate | blockAlign | bits | data | dataSize`。
/// 其中 `riffSize = 36 + data_size`。
pub fn build_wav_header(channels: u16, rate: u32, bits: u16, data_size: u32) -> [u8; WAV_HEADER_LEN] {
    let block_align: u16 = channels * bits.div_ceil(8);
    let byte_rate: u32 = rate * block_align as u32;

    let mut h = [0u8; WAV_HEADER_LEN];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36u32.wrapping_add(data_size)).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // WAVE_FORMAT_PCM
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&bits.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_size.to_le_bytes());
    h
}

/// 采集链路的默认流头：16 bit PCM + 假 `data` 长度。
pub fn build_stream_wav_header(channels: u16, rate: u32) -> [u8; WAV_HEADER_LEN] {
    build_wav_header(channels, rate, 16, FAKE_DATA_SIZE)
}

/// 写一个长度真实的 WAV 文件（给测试 / 调试用）。
pub fn write_wav(
    path: impl AsRef<Path>,
    channels: u16,
    rate: u32,
    bits: u16,
    pcm: &[u8],
) -> std::io::Result<()> {
    let len = u32::try_from(pcm.len()).unwrap_or(u32::MAX);
    let header = build_wav_header(channels, rate, bits, len);
    let mut f = std::fs::File::create(path)?;
    f.write_all(&header)?;
    f.write_all(pcm)?;
    f.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32le(b: &[u8]) -> u32 {
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }
    fn u16le(b: &[u8]) -> u16 {
        u16::from_le_bytes([b[0], b[1]])
    }

    #[test]
    fn header_is_44_bytes_and_well_formed() {
        let h = build_wav_header(2, 44_100, 16, FAKE_DATA_SIZE);
        assert_eq!(h.len(), 44);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(&h[12..16], b"fmt ");
        assert_eq!(&h[36..40], b"data");
    }

    #[test]
    fn header_field_values_44100_stereo_16bit() {
        let h = build_wav_header(2, 44_100, 16, FAKE_DATA_SIZE);
        // RIFF size = 36 + dataSize
        assert_eq!(u32le(&h[4..8]), 36 + FAKE_DATA_SIZE);
        assert_eq!(u32le(&h[4..8]), 0x7FFF_FF24);
        assert_eq!(u32le(&h[16..20]), 16); // fmt chunk size
        assert_eq!(u16le(&h[20..22]), 1); // PCM
        assert_eq!(u16le(&h[22..24]), 2); // channels
        assert_eq!(u32le(&h[24..28]), 44_100); // samplerate
        assert_eq!(u32le(&h[28..32]), 44_100 * 4); // byteRate = rate * blockAlign
        assert_eq!(u16le(&h[32..34]), 4); // blockAlign
        assert_eq!(u16le(&h[34..36]), 16); // bits
        assert_eq!(u32le(&h[40..44]), 0x7FFF_FF00); // fake data size
    }

    #[test]
    fn header_stream_default_matches_python_reference_bytes() {
        // pc_stereo_hub.py 里 struct.pack('<4sI4s4sIHHIIHH4sI', b'RIFF', 0x7FFFFF24,
        //   b'WAVE', b'fmt ', 16, 1, 2, 44100, 176400, 4, 16, b'data', 0x7FFFFF00)
        let expected: Vec<u8> = {
            let mut v = Vec::new();
            v.extend_from_slice(b"RIFF");
            v.extend_from_slice(&0x7FFF_FF24u32.to_le_bytes());
            v.extend_from_slice(b"WAVE");
            v.extend_from_slice(b"fmt ");
            v.extend_from_slice(&16u32.to_le_bytes());
            v.extend_from_slice(&1u16.to_le_bytes());
            v.extend_from_slice(&2u16.to_le_bytes());
            v.extend_from_slice(&44_100u32.to_le_bytes());
            v.extend_from_slice(&176_400u32.to_le_bytes());
            v.extend_from_slice(&4u16.to_le_bytes());
            v.extend_from_slice(&16u16.to_le_bytes());
            v.extend_from_slice(b"data");
            v.extend_from_slice(&0x7FFF_FF00u32.to_le_bytes());
            v
        };
        assert_eq!(build_stream_wav_header(2, 44_100).to_vec(), expected);
    }

    #[test]
    fn header_real_size_and_mono() {
        let h = build_wav_header(1, 8_000, 8, 100);
        assert_eq!(u32le(&h[4..8]), 136);
        assert_eq!(u16le(&h[22..24]), 1);
        assert_eq!(u32le(&h[28..32]), 8_000); // byteRate = 8000*1*1
        assert_eq!(u16le(&h[32..34]), 1);
        assert_eq!(u16le(&h[34..36]), 8);
        assert_eq!(u32le(&h[40..44]), 100);
    }

    /// 单元测试用的可写目录。
    ///
    /// 不用 `std::env::temp_dir()`：在某些受限环境里 `%TEMP%` 对子进程不可写
    /// （实测 `create_dir_all` 返回 os error 5 / PermissionDenied）。
    /// cargo 一定会写 workspace 的 `target/`，所以那里必定可写。
    fn test_tmp_dir() -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-tmp");
        std::fs::create_dir_all(&dir).expect("创建 target/test-tmp");
        dir
    }

    #[test]
    fn write_wav_roundtrip() {
        let dir = test_tmp_dir();
        let p = dir.join("hdr.wav");
        let pcm = vec![0u8; 40];
        write_wav(&p, 2, 44_100, 16, &pcm).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(bytes.len(), 44 + 40);
        assert_eq!(u32le(&bytes[40..44]), 40);
        assert_eq!(u32le(&bytes[4..8]), 36 + 40);
    }
}
