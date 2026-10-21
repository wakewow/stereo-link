//! 本机声卡输出。
//!
//! AirPlay 库在自己的线程上回调 `audio_process(&[f32])`（交错、±1.0 归一化），
//! 这里把它丢进一个抖动缓冲，再由 cpal 的 WASAPI 回调线程按声卡节拍取走。
//!
//! 两个线程的职责：
//!   - 生产者（AirPlay 回调线程）：重采样 + 声道映射 + 入队，绝不阻塞；
//!   - 消费者（WASAPI 回调线程）：只做出队和 `f32 → 声卡采样格式` 的转换，绝不分堆。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use shairplay::{AudioFormat, AudioSession};

/// 抖动缓冲目标水位：攒够 200 ms 才开口，播放中掉到 20 ms 以下就重新攒。
/// 这点延迟听音乐无感，但能避免每秒几十次的欠载爆音。
const TARGET_MS: u32 = 200;
const RESUME_MS: u32 = 20;
/// 缓冲上限 3 秒：超出就丢最旧的数据，宁可跳一下也不要越积越晚。
const MAX_MS: u32 = 3000;

fn lock_ignore_poison<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // 音频线程里 panic 会污染锁；这里绝不因为上一次出错就整条链路失效。
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Jitter {
    samples: VecDeque<f32>,
    max_samples: usize,
    start_samples: usize,
    resume_samples: usize,
    playing: bool,
}

impl Jitter {
    fn new(channels: usize, rate: u32) -> Self {
        let ch = channels.max(1);
        Self {
            samples: VecDeque::with_capacity(rate as usize * ch * 2),
            max_samples: rate as usize * ch * MAX_MS as usize / 1000,
            start_samples: rate as usize * ch * TARGET_MS as usize / 1000,
            resume_samples: rate as usize * ch * RESUME_MS as usize / 1000,
            playing: false,
        }
    }

    fn push(&mut self, data: &[f32]) {
        self.samples.extend(data.iter().copied());
        if self.samples.len() > self.max_samples {
            let excess = self.samples.len() - self.max_samples;
            self.samples.drain(..excess);
        }
    }

    fn clear(&mut self) {
        self.samples.clear();
        self.playing = false;
    }

    /// 用缓冲里的数据填满 `out`；数据不够就填静音并回到攒数据状态。
    fn fill(&mut self, out: &mut [f32]) {
        out.fill(0.0);

        if !self.playing {
            if self.samples.len() < self.start_samples {
                return;
            }
            self.playing = true;
        }

        // 欠载：整块放弃，等下次攒够。半块混静音会让音乐听起来发抖。
        if self.samples.len() < out.len() {
            self.playing = false;
            return;
        }

        for slot in out.iter_mut() {
            *slot = self.samples.pop_front().unwrap_or(0.0);
        }

        if self.samples.len() < self.resume_samples {
            self.playing = false;
        }
    }
}

/// 一路输出设备。克隆代价只有一次 `Arc` 自增，可以塞进 `Handler` 里到处传。
#[derive(Clone)]
pub struct Output {
    jitter: Arc<Mutex<Jitter>>,
    gain: Arc<AtomicU32>,
    rate: u32,
    channels: usize,
}

impl Output {
    /// 打开输出设备并启动播放流。返回的 `Stream` 必须被调用方持有——drop 掉就静音。
    pub fn open(selector: Option<&str>, rate: u32, channels: Option<u16>) -> Result<(Self, cpal::Stream, String)> {
        let host = cpal::default_host();
        let (device, name) = match selector {
            Some(want) => {
                let device = find_device(&host, want)?;
                let name = device_name(&device);
                (device, name)
            }
            None => {
                let d = host
                    .default_output_device()
                    .ok_or_else(|| anyhow!("系统里没有可用的声音输出设备"))?;
                let name = device_name(&d);
                (d, name)
            }
        };

        // 采样格式跟着设备默认走（绝大多数是 F32），采样率由我们指定：
        // cpal 的 WASAPI 后端在共享模式下会带上 AUTOCONVERTPCM|SRC_DEFAULT_QUALITY，
        // 所以哪怕设备混音格式是 48 kHz，这里开 44.1 kHz 也由系统负责转换。
        let supported = device
            .default_output_config()
            .with_context(|| format!("读不到「{name}」的默认输出格式"))?;
        let channels = channels.unwrap_or_else(|| supported.channels()).max(1);
        let config = StreamConfig {
            channels,
            sample_rate: rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let output = Self {
            jitter: Arc::new(Mutex::new(Jitter::new(channels as usize, rate))),
            gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            rate,
            channels: channels as usize,
        };

        let stream = build_stream(&device, &config, supported.sample_format(), output.clone())
            .with_context(|| format!("在「{name}」上开 {rate} Hz / {channels} ch 的输出流出错"))?;
        stream.play().context("启动声卡播放流失败")?;

        Ok((output, stream, name))
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// AirPlay 的 volume 是 dB（0.0 = 最大，-144.0 = 静音），换成线性增益。
    pub fn set_volume_db(&self, db: f32) {
        let db = db.clamp(-144.0, 0.0);
        let gain = if db <= -60.0 { 0.0 } else { 10f32.powf(db / 20.0) };
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    fn push(&self, data: &[f32]) {
        lock_ignore_poison(&self.jitter).push(data);
    }

    fn flush(&self) {
        lock_ignore_poison(&self.jitter).clear();
    }
}

fn build_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    format: SampleFormat,
    output: Output,
) -> Result<cpal::Stream> {
    fn make<T>(
        device: &cpal::Device,
        config: &StreamConfig,
        output: Output,
    ) -> Result<cpal::Stream, cpal::BuildStreamError>
    where
        T: cpal::SizedSample + cpal::FromSample<f32>,
    {
        let jitter = output.jitter.clone();
        let gain = output.gain.clone();
        let mut scratch: Vec<f32> = Vec::new();
        let err_fn = |err| tracing::warn!(%err, "声卡回调报错");

        device.build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                if scratch.len() != data.len() {
                    scratch.resize(data.len(), 0.0);
                }
                lock_ignore_poison(&jitter).fill(&mut scratch);
                let g = f32::from_bits(gain.load(Ordering::Relaxed));
                for (slot, sample) in data.iter_mut().zip(scratch.iter()) {
                    *slot = T::from_sample(*sample * g);
                }
            },
            err_fn,
            None,
        )
    }

    match format {
        SampleFormat::F32 => make::<f32>(device, config, output),
        SampleFormat::F64 => make::<f64>(device, config, output),
        SampleFormat::I16 => make::<i16>(device, config, output),
        SampleFormat::U16 => make::<u16>(device, config, output),
        SampleFormat::I32 => make::<i32>(device, config, output),
        SampleFormat::U32 => make::<u32>(device, config, output),
        other => return Err(anyhow!("暂不支持的声卡采样格式：{other}")),
    }
    .map_err(anyhow::Error::from)
}

/// 列出本机所有输出设备（`airplay-recv --list-devices` 用）。
///
/// 注意：WASAPI 端点常常重名（同型号音箱、或者都叫「扬声器」），所以这里
/// 同时打印设备 id；重名时用 id 选，最稳。
pub fn list_devices() -> Result<()> {
    let host = cpal::default_host();
    let default_id = host.default_output_device().and_then(|d| device_id(&d));
    println!("本机输出设备（--device 可以写 #序号、完整 id，或名字的一部分）：");
    for (index, device) in host.output_devices().context("枚举输出设备失败")?.enumerate() {
        let name = device_name(&device);
        let id = device_id(&device).unwrap_or_else(|| "<无 id>".to_string());
        let format = device
            .default_output_config()
            .map(|c| format!("{} Hz / {} ch", c.sample_rate(), c.channels()))
            .unwrap_or_else(|_| "格式未知".to_string());
        let mark = if Some(&id) == default_id.as_ref() {
            "  ← 系统默认"
        } else {
            ""
        };
        println!("  {:>2}. {name}（{format}）{mark}", index + 1);
        println!("      id={id}");
    }
    Ok(())
}

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<未知声卡>".to_string())
}

fn device_id(device: &cpal::Device) -> Option<String> {
    device.id().ok().map(|id| id.to_string())
}

/// 挑设备：`default` = 系统默认；`#N` = --list-devices 里的第 N 个；
/// 否则先按 id 全等匹配，再按名字子串匹配。
/// 名字命中多个时不瞎猜，直接报错让用户改用 id 或 #序号（WASAPI 里重名很常见）。
fn find_device(host: &cpal::Host, want: &str) -> Result<cpal::Device> {
    if want.eq_ignore_ascii_case("default") {
        return host
            .default_output_device()
            .ok_or_else(|| anyhow!("系统里没有可用的声音输出设备"));
    }

    if let Some(rest) = want.strip_prefix('#') {
        let index: usize = rest
            .parse()
            .with_context(|| format!("{want} 不是合法序号，例如 #3"))?;
        return host
            .output_devices()
            .context("枚举输出设备失败")?
            .nth(index.saturating_sub(1))
            .ok_or_else(|| anyhow!("没有第 {index} 个输出设备（用 --list-devices 看序号）"));
    }

    let want_lower = want.to_lowercase();
    let mut by_name = Vec::new();
    let mut all = Vec::new();
    for dev in host.output_devices().context("枚举输出设备失败")? {
        let name = device_name(&dev);
        let id = device_id(&dev);
        if let Some(id) = &id {
            if id.to_lowercase() == want_lower {
                return Ok(dev);
            }
        }
        if name.to_lowercase().contains(&want_lower) {
            by_name.push((dev, name, id));
        } else {
            all.push(format!("{name}｜id={}", id.unwrap_or_else(|| "<无 id>".to_string())));
        }
    }

    match by_name.len() {
        1 => Ok(by_name.pop().expect("len==1").0),
        0 => Err(anyhow!(
            "找不到匹配「{want}」的输出设备；现有设备：{}",
            if all.is_empty() {
                "（无）".to_string()
            } else {
                all.join("、")
            }
        )),
        _ => {
            let list = by_name
                .iter()
                .map(|(_, name, id)| format!("{name}｜id={}", id.clone().unwrap_or_else(|| "<无 id>".to_string())))
                .collect::<Vec<_>>()
                .join("\n    ");
            Err(anyhow!(
                "「{want}」匹配到 {} 个输出设备，请改用 id 指定：\n    {list}",
                by_name.len()
            ))
        }
    }
}

/// 单路程线性插值重采样器。
///
/// 只用来对齐速率（AirPlay 常见 44.1 kHz，声卡可能是 48 kHz）。线性插值的高频
/// 保真度不如 rubato 那类 sinc 重采样，但胜在零依赖、延迟低；真要追求音质再换。
struct LinearResampler {
    /// 每产出一个目标帧，源侧前进多少帧
    step: f64,
    /// 当前源侧位置（相对于 `tail` 的帧偏移）
    pos: f64,
    channels: usize,
    tail: Vec<f32>,
}

impl LinearResampler {
    fn new(src_rate: u32, dst_rate: u32, channels: usize) -> Self {
        Self {
            step: src_rate as f64 / dst_rate as f64,
            pos: 0.0,
            channels: channels.max(1),
            tail: Vec::new(),
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        if input.len() < ch {
            return;
        }

        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(input);
        let frames = buf.len() / ch;
        if frames < 2 {
            self.tail = buf;
            return;
        }

        let mut pos = self.pos;
        while pos + 1.0 < frames as f64 {
            let i = pos as usize;
            let frac = (pos - i as f64) as f32;
            for c in 0..ch {
                let a = buf[i * ch + c];
                let b = buf[(i + 1) * ch + c];
                out.push(a + (b - a) * frac);
            }
            pos += self.step;
        }

        // 保留最后一帧未消费的样本，作为下一块的插值起点。
        let keep = (pos as usize).min(frames - 1);
        self.tail = buf[keep * ch..].to_vec();
        self.pos = pos - keep as f64;
    }
}

/// 声道数对齐：少了就复制，多了就丢掉多余声道。
fn map_channels(input: &[f32], src: usize, dst: usize) -> Vec<f32> {
    if src == dst || src == 0 {
        return input.to_vec();
    }
    let frames = input.len() / src;
    let mut out = Vec::with_capacity(frames * dst);
    for frame in input.chunks_exact(src) {
        for c in 0..dst {
            out.push(frame[if c < src { c } else { c % src }]);
        }
    }
    out
}

/// 一路 AirPlay 音频流的接收会话。库每条流调一次 `new`，断流时 drop。
pub struct Session {
    output: Output,
    resampler: Option<LinearResampler>,
    src_channels: usize,
    dst_channels: usize,
}

impl Session {
    pub fn new(output: Output, format: &AudioFormat) -> Self {
        let src_channels = (format.channels as usize).max(1);
        let needs_resample = format.sample_rate != output.rate();
        tracing::info!(
            src_ch = src_channels,
            src_rate = format.sample_rate,
            dst_ch = output.channels(),
            dst_rate = output.rate(),
            resample = needs_resample,
            "开始接收音频流"
        );
        Self {
            resampler: needs_resample.then(|| {
                LinearResampler::new(format.sample_rate, output.rate(), src_channels)
            }),
            output: output.clone(),
            src_channels,
            dst_channels: output.channels(),
        }
    }
}

impl AudioSession for Session {
    fn audio_process(&mut self, samples: &[f32]) {
        let mut resampled = Vec::with_capacity(samples.len());
        match self.resampler.as_mut() {
            Some(rs) => rs.process(samples, &mut resampled),
            None => resampled.extend_from_slice(samples),
        }
        if resampled.is_empty() {
            return;
        }
        let mapped = map_channels(&resampled, self.src_channels, self.dst_channels);
        self.output.push(&mapped);
    }

    fn audio_flush(&mut self) {
        // 跳转/暂停时清空积压，不然会听到上一段音乐的尾巴。
        self.output.flush();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.output.flush();
        tracing::info!("音频流结束");
    }
}
