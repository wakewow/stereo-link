//! `miplay-bridge stream` —— 把本机**系统声音**（WASAPI 回环）实时推给 AirPlay 2 音箱。
//!
//! ## 为什么是独立子命令，而不是 `serve` 下的一条路由
//!
//! 1. 这条通路**完全在局域网内，不需要小米账号**：`AirPlaySender` 用 HAP 瞬时配对直连音箱
//!    （见 `crates/miplay-airplay/README.md`）。做成 `serve` 的兄弟命令后，没有凭据的机器也能用，
//!    而 `serve` 一启动就要 `MiToken::load_default()`。
//! 2. 它是**独占会话**：从 `RECORD` 到 `TEARDOWN` 之间音箱在放我们的流。这种生命周期必须跟着
//!    **进程**走；挂在 HTTP 请求上会出现「请求断了但音箱还在响」。
//! 3. `Ctrl-C` 必须能立刻 `TEARDOWN`。命令行进程的退出路径最短、最可控。
//!
//! 这条链路的延迟来源与实测数据见 `docs/end-to-end.md`。
//!
//! ## 防火墙是硬前提
//!
//! 音箱会往我们的 UDP 临时端口回送 NTP 时序包，`SETUP` 要等这个包被应答。入站 UDP 被拦时
//! `StreamSetup` 会卡到 10060 超时。见 `docs/firewall-requirement.md`。

#[cfg(not(windows))]
pub async fn run(_argv: &[String]) -> anyhow::Result<()> {
    anyhow::bail!("`stream` 依赖 WASAPI 回环采集，只在 Windows 上可用")
}

#[cfg(windows)]
pub use win::run;

#[cfg(windows)]
mod win {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context, Result};
    use miplay_airplay::{
        apply_gain_s16le, AirPlaySender, Codec, SenderConfig, Status, DEVICE_LATENCY_MAX_MS,
        DEVICE_LATENCY_MIN_MS, TARGET_TOTAL_MS,
    };
    use miplay_audio::{
        list_endpoints, resolve_endpoint, AudioCapture, CaptureConfig, CaptureMode, DeviceSelector,
    };

    /// 音箱的 AirPlay 端口。
    const DEFAULT_PORT: u16 = 7000;
    /// 默认目标：Xiaomi Sound-左（L16A），task-7 实测的原生 AirPlay 2 接收端。
    const DEFAULT_TARGET: &str = "192.168.1.21:7000";
    /// 采集线程 → 推流循环之间的队列深度（块）。8 块 × 10 ms ≈ 80 ms 上限，
    /// 稳态下远小于此（见 `docs/end-to-end.md` 的排队实测）。
    const CAPTURE_QUEUE_CHUNKS: usize = 8;
    /// 一次 `read_vec` 的最长等待；也是 `Ctrl-C` 后采集线程最晚退出的时间。
    const READ_TIMEOUT: Duration = Duration::from_millis(200);
    /// 格式固定：音箱的实时流窗口以 44.1 kHz 立体声计。
    const SAMPLE_RATE: u32 = 44_100;
    const CHANNELS: u16 = 2;
    /// 44.1 kHz/2ch/s16le 每秒字节数。
    const BYTES_PER_SECOND: f64 = SAMPLE_RATE as f64 * CHANNELS as f64 * 2.0;
    /// 采集线程回来的块大小（毫秒）。10 ms ≈ 1.25 个 RTP 包，足够平滑又不会自己造延迟。
    const DEFAULT_CHUNK_MS: u32 = 10;
    /// WASAPI 共享缓冲时长（毫秒）。**采集侧延迟的主要旋钮**：默认 20 ms 而不是 200 ms。
    const DEFAULT_BUFFER_MS: u32 = 20;

    const HELP: &str = "\
miplay-bridge stream —— 把本机系统声音实时推给 AirPlay 2 音箱

用法:
  miplay-bridge stream [选项]

选项:
  --target IP[:端口]     音箱地址，默认 192.168.1.21:7000
  --device <子串>        选哪个输出端点做回环采集（默认端点可省略）
  --gain <倍数>          线性增益 0.0~8.0，默认 1.0（饱和，不回绕）
  --latency-ms <毫秒>    在同步包里声明的延迟，250~2000，默认 250（窗口下限）
  --codec alac|lpcm      音频编码，默认 alac（与真机实测路径一致）
  --volume <0.0~1.0>     可选：推流前设置音箱音量（映射成 AirPlay 的 dB）
  --seconds <秒>         可选：到点自动收尾（默认一直跑到 Ctrl-C）
  --chunk-ms <毫秒>      采集块大小，默认 10
  --buffer-ms <毫秒>     WASAPI 共享缓冲，20~2000，默认 20
  --stats-interval <毫秒> 统计打印间隔，默认 1000
  --dry-run              只枚举端点、打印计划，不打开采集也不连音箱

Ctrl-C 会先发 TEARDOWN 再退出；退出时打印完整统计。
";

    /// 命令行参数（解析后）。
    #[derive(Debug, Clone)]
    struct Args {
        target: SocketAddr,
        target_is_default: bool,
        device: Option<String>,
        gain: f32,
        latency_ms: u32,
        codec: Codec,
        volume: Option<f32>,
        seconds: Option<f64>,
        chunk_ms: u32,
        buffer_ms: u32,
        stats_interval: Duration,
        dry_run: bool,
    }

    pub async fn run(argv: &[String]) -> Result<()> {
        if argv.iter().any(|a| a == "-h" || a == "--help") {
            println!("{HELP}");
            return Ok(());
        }
        let args = Args::parse(argv)?;

        let selector = match &args.device {
            Some(needle) => DeviceSelector::name_contains(needle.clone()),
            None => DeviceSelector::Default,
        };
        // 先解析端点：失败时还没碰过音箱，退出最干净。
        let endpoint = resolve_endpoint(&selector, CaptureMode::Loopback).with_context(|| {
            format!(
                "解析回环采集端点失败（选择器 {}；用 --dry-run 看本机有哪些端点）",
                selector.describe()
            )
        })?;

        println!("== miplay-bridge stream ==");
        println!(
            "目标音箱     {}{}",
            args.target,
            if args.target_is_default {
                "（默认，Xiaomi Sound-左）"
            } else {
                ""
            }
        );
        println!("采集端点     {}", endpoint.describe());
        println!(
            "采集格式     {} Hz / {} ch / 16 bit —— 块 {} ms，WASAPI 缓冲 {} ms，环形缓冲 {} 秒",
            SAMPLE_RATE,
            CHANNELS,
            args.chunk_ms,
            args.buffer_ms,
            CaptureConfig::default().ring_seconds
        );
        println!(
            "声明延迟     {} ms（{} 帧，窗口 {}~{} ms）",
            args.latency_ms,
            miplay_airplay::latency::ms_to_frames(args.latency_ms, SAMPLE_RATE),
            DEVICE_LATENCY_MIN_MS,
            DEVICE_LATENCY_MAX_MS
        );
        println!("编码方式     {}", args.codec.as_str());
        println!(
            "增益         {:.2}×（上限 {:.1}×）",
            args.gain,
            miplay_airplay::MAX_GAIN
        );
        if let Some(v) = args.volume {
            println!("音箱音量     推流前设为 {v:.2}（0.0~1.0）");
        }
        println!("延迟目标     ≤ {TARGET_TOTAL_MS:.0} ms 端到端");

        if args.dry_run {
            let end = list_endpoints().context("枚举音频端点失败")?;
            println!("\n本机活动端点（{} 个）:", end.len());
            for ep in &end {
                println!("  {}", ep.describe());
                if let Some(err) = &ep.error {
                    println!("      ⚠ 无法读混音格式：{err}");
                }
            }
            println!("\n--dry-run：只枚举，不打开采集、不连接音箱。");
            return Ok(());
        }

        // --- 1. 先开采集：音频有问题时不要先把音箱占住 ---
        let mut capture = AudioCapture::new(capture_config(&args))
            .context("创建回环采集失败（用 --dry-run 看端点）")?;
        capture.start().context("启动回环采集失败")?;
        if let Some(fmt) = capture.negotiated_format() {
            println!("\n已协商格式   {}", fmt.describe());
        }
        if let Some(name) = capture.device_name() {
            println!("实际端点     {name}");
        }

        // --- 2. 连音箱并进入推流状态 ---
        let sender_cfg = SenderConfig::new("miplay-bridge stream")
            .with_latency_ms(args.latency_ms)
            .with_codec(args.codec);
        sender_cfg.validate().context("发送端配置非法")?;

        let connect_started = Instant::now();
        let mut sender = AirPlaySender::connect(args.target, sender_cfg)
            .await
            .with_context(|| {
                format!(
                    "连接/建流到 {} 失败。检查：音箱在线？UDP 入站放行（docs/firewall-requirement.md）？\
                     目标是不是 AirPlay 接收端（只有 .21/L16A 广播 _airplay._tcp）？",
                    args.target
                )
            })?;
        println!(
            "\n建流成功     {:.0} ms（含 HAP 瞬时配对）",
            connect_started.elapsed().as_secs_f64() * 1000.0
        );
        {
            let info = sender.session_info();
            println!(
                "RTP 端口     数据 {} 控制 {}",
                opt(info.data_port),
                opt(info.control_port)
            );
            println!(
                "设备上报     流 id={} 音频缓冲={} 到达→渲染={}",
                opt(info.stream_id),
                opt(info.audio_buffer_size),
                opt(info.arrival_to_render_latency_ms)
            );
            if !info.steps.is_empty() {
                println!("RTSP 步骤    {}", info.steps.join(" | "));
            }
        }
        println!("延迟预算     {}", sender.latency_budget().summary(true));

        if let Some(v) = args.volume {
            match sender.set_volume(v).await {
                Ok(()) => println!("音箱音量     已下发 {v:.2}"),
                // 真机实测：这台固件偶发 500，但不影响推流，所以只警告。
                Err(e) => eprintln!("⚠ 设置音量失败（继续推流）：{e:#}"),
            }
        }

        // --- 3. 采集线程：WASAPI 阻塞读，经有界通道交给异步推流循环 ---
        let running = Arc::new(AtomicBool::new(true));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(CAPTURE_QUEUE_CHUNKS);
        let chunk_bytes = aligned_chunk_bytes(args.chunk_ms);
        let capture_thread = {
            let running = Arc::clone(&running);
            std::thread::Builder::new()
                .name("wasapi-loopback".into())
                .spawn(move || -> AudioCapture {
                    while running.load(Ordering::Relaxed) {
                        match capture.read_vec(chunk_bytes, READ_TIMEOUT) {
                            // 空 = 这段时间没采到数据（系统静音时也可能这样），不是错误
                            Ok(buf) if buf.is_empty() => {}
                            Ok(buf) => {
                                // 推流侧满了就阻塞在这里——反压一路传回 WASAPI 环形缓冲，
                                // 宁可丢老音频，也不让排队延迟无界增长。
                                if tx.blocking_send(buf).is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                eprintln!("采集读取失败：{e}");
                                break;
                            }
                        }
                    }
                    capture
                })
                .context("启动采集线程失败")?
        };

        // --- 4. 推流主循环 ---
        let started = Instant::now();
        let mut ticker = tokio::time::interval(args.stats_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut fed_bytes: u64 = 0;
        let mut window = Meter::default();
        let mut total = Meter::default();
        let mut reason = String::from("Ctrl-C");

        /// `select!` 只负责「等事件」，碰 `sender` 的逻辑全放到 `match` 里：
        /// 借用检查器不允许同一个 `select!` 的分支里既有 `sender.stats()`（不可变借用）
        /// 又有 `sender.send_pcm()`（可变借用）。
        enum Event {
            CtrlC,
            Tick,
            Chunk(Option<Vec<u8>>),
        }

        loop {
            let event = tokio::select! {
                _ = tokio::signal::ctrl_c() => Event::CtrlC,
                _ = ticker.tick() => Event::Tick,
                chunk = rx.recv() => Event::Chunk(chunk),
            };

            match event {
                Event::CtrlC => {
                    // reason 保持初值 "Ctrl-C"，不需要重写
                    break;
                }
                Event::Tick => {
                    let s = sender.stats();
                    if s.packets_sent > 0 || window.samples > 0 {
                        println!(
                            "[t={:>6.1}s] 包={} 帧={} 排队={:.1}ms 欠载={} 丢弃={}B 峰值={:.3}FS RMS={:.3}FS",
                            started.elapsed().as_secs_f64(),
                            s.packets_sent,
                            s.frames_sent,
                            bytes_to_ms(s.queued_bytes),
                            s.underrun_packets,
                            s.dropped_pcm_bytes,
                            window.peak_fs(),
                            window.rms_fs(),
                        );
                    }
                    if let Status::Failed(why) = sender.status() {
                        eprintln!("发送端失败：{why}");
                        reason = "发送端失败".to_string();
                        break;
                    }
                    total.absorb(&window);
                    window.reset();
                }
                Event::Chunk(chunk) => {
                    let Some(mut chunk) = chunk else {
                        reason = "采集线程结束".to_string();
                        break;
                    };
                    if let Err(e) = apply_gain_s16le(&mut chunk, args.gain) {
                        eprintln!("增益处理失败：{e}");
                        reason = "增益处理失败".to_string();
                        break;
                    }
                    window.add(&chunk);
                    fed_bytes += chunk.len() as u64;
                    if let Err(e) = sender.send_pcm(&chunk).await {
                        eprintln!("喂入 PCM 失败：{e:#}");
                        reason = "发送线程退出".to_string();
                        break;
                    }
                }
            }

            if let Some(secs) = args.seconds {
                if started.elapsed().as_secs_f64() >= secs {
                    reason = format!("--seconds {secs} 到点");
                    break;
                }
            }
        }

        // --- 5. 收尾：先停采集，再 TEARDOWN（顺序反了会在音箱上留下静音尾巴） ---
        running.store(false, Ordering::Relaxed);
        drop(rx); // 让还阻塞在 blocking_send 上的采集线程立刻返回
        let mut capture = match capture_thread.join() {
            Ok(c) => c,
            Err(_) => bail!("采集线程 panic"),
        };
        capture.stop();
        total.absorb(&window);

        let stats = sender.stats();
        let budget = sender.latency_budget();
        let info = sender.session_info().clone();
        let teardown_started = Instant::now();
        sender.stop().await.context("TEARDOWN 失败")?;
        let teardown_ms = teardown_started.elapsed().as_secs_f64() * 1000.0;

        let wall = started.elapsed().as_secs_f64();
        println!("\n=== 结束（{reason}）===");
        println!("运行时长     {wall:.2} s");
        println!(
            "喂入 PCM     {} B（{:.2} s @ {} Hz/{} ch/s16le）",
            fed_bytes,
            fed_bytes as f64 / BYTES_PER_SECOND,
            SAMPLE_RATE,
            CHANNELS
        );
        println!(
            "实发音频     {} 包 / {} 帧 / {} B，同步包 {}",
            stats.packets_sent, stats.frames_sent, stats.bytes_sent, stats.syncs_sent
        );
        println!(
            "排队/欠载/丢弃 排队 {:.1} ms，欠载 {} 包，丢弃 {} B",
            bytes_to_ms(stats.queued_bytes),
            stats.underrun_packets,
            stats.dropped_pcm_bytes
        );
        println!(
            "信号         峰值 {:.3} FS，RMS {:.3} FS（0 = 采到的是静音）",
            total.peak_fs(),
            total.rms_fs()
        );
        println!(
            "设备上报     流 id={} 音频缓冲={} 到达→渲染={}",
            opt(info.stream_id),
            opt(info.audio_buffer_size),
            opt(info.arrival_to_render_latency_ms)
        );
        println!("RTSP 步骤    {}", info.steps.join(" | "));
        println!("TEARDOWN     {teardown_ms:.0} ms");
        println!("延迟预算     {}", budget.summary(true));
        println!(
            "             （估算 {:.0} ms，距 {TARGET_TOTAL_MS:.0} ms 目标 {:.0} ms；\
             `到达→渲染` 未上报时那部分用的是估计值）",
            budget.estimated_total_ms(true),
            budget.gap_to_target_ms(true)
        );

        if fed_bytes == 0 {
            bail!("一帧都没喂出去：采集侧没数据（选错端点？系统完全静音？）");
        }
        Ok(())
    }

    impl Args {
        fn parse(argv: &[String]) -> Result<Self> {
            const KNOWN: &[&str] = &[
                "--target",
                "--device",
                "--gain",
                "--latency-ms",
                "--codec",
                "--volume",
                "--seconds",
                "--chunk-ms",
                "--buffer-ms",
                "--stats-interval",
                "--dry-run",
            ];
            for a in argv {
                if a.starts_with("--") && !KNOWN.contains(&a.as_str()) {
                    bail!("未知参数 `{a}`\n\n{HELP}");
                }
            }

            let (target, target_is_default) = match flag(argv, "--target") {
                Some(s) => (parse_target(&s)?, false),
                None => (
                    DEFAULT_TARGET.parse().expect("默认目标字面量合法"),
                    true,
                ),
            };
            let gain: f32 = match flag(argv, "--gain") {
                Some(s) => number(&s, "--gain")?,
                None => 1.0,
            };
            if !gain.is_finite() || !(0.0..=miplay_airplay::MAX_GAIN).contains(&gain) {
                bail!(
                    "--gain 必须是 0.0..={} 的有限值，收到 {gain}",
                    miplay_airplay::MAX_GAIN
                );
            }
            let latency_ms = match flag(argv, "--latency-ms") {
                Some(s) => number::<u32>(&s, "--latency-ms")?,
                None => DEVICE_LATENCY_MIN_MS,
            };
            if !(DEVICE_LATENCY_MIN_MS..=DEVICE_LATENCY_MAX_MS).contains(&latency_ms) {
                bail!(
                    "--latency-ms 必须在 {}~{}（设备声明的窗口），收到 {latency_ms}",
                    DEVICE_LATENCY_MIN_MS,
                    DEVICE_LATENCY_MAX_MS
                );
            }
            let codec = match flag(argv, "--codec").as_deref() {
                None | Some("alac") => Codec::Alac,
                Some("lpcm") | Some("l16") => Codec::Lpcm,
                Some(other) => bail!("--codec 只能是 alac 或 lpcm，收到 {other}"),
            };
            let volume = match flag(argv, "--volume") {
                Some(s) => {
                    let v: f32 = number(&s, "--volume")?;
                    if !v.is_finite() || !(0.0..=1.0).contains(&v) {
                        bail!("--volume 必须在 0.0~1.0，收到 {v}");
                    }
                    Some(v)
                }
                None => None,
            };
            let seconds = match flag(argv, "--seconds") {
                Some(s) => Some(number::<f64>(&s, "--seconds")?),
                None => None,
            };
            let chunk_ms = match flag(argv, "--chunk-ms") {
                Some(s) => number::<u32>(&s, "--chunk-ms")?,
                None => DEFAULT_CHUNK_MS,
            };
            if !(1..=100).contains(&chunk_ms) {
                bail!("--chunk-ms 必须在 1~100，收到 {chunk_ms}");
            }
            let buffer_ms = match flag(argv, "--buffer-ms") {
                Some(s) => number::<u32>(&s, "--buffer-ms")?,
                None => DEFAULT_BUFFER_MS,
            };
            if !(20..=2_000).contains(&buffer_ms) {
                bail!("--buffer-ms 必须在 20~2000（WASAPI 共享模式下限），收到 {buffer_ms}");
            }
            let stats_interval = Duration::from_millis(match flag(argv, "--stats-interval") {
                Some(s) => number::<u64>(&s, "--stats-interval")?,
                None => 1_000,
            });
            if stats_interval.is_zero() {
                bail!("--stats-interval 必须大于 0（tokio 定时器不接受零间隔）");
            }

            Ok(Self {
                target,
                target_is_default,
                device: flag(argv, "--device"),
                gain,
                latency_ms,
                codec,
                volume,
                seconds,
                chunk_ms,
                buffer_ms,
                stats_interval,
                dry_run: argv.iter().any(|a| a == "--dry-run"),
            })
        }
    }

    fn capture_config(args: &Args) -> CaptureConfig {
        let mut cfg = CaptureConfig::loopback_default()
            .with_mode(CaptureMode::Loopback)
            .with_samplerate(SAMPLE_RATE)
            .with_channels(CHANNELS);
        cfg.buffer_ms = args.buffer_ms;
        cfg.ring_seconds = 2.0; // 实时推流只需要很短的环形缓冲：慢读者就该丢帧
        if let Some(needle) = &args.device {
            cfg = cfg.with_device_name(needle.clone());
        }
        cfg
    }

    /// `--target` 接受 `IP` 或 `IP:端口`。
    fn parse_target(s: &str) -> Result<SocketAddr> {
        if let Ok(addr) = s.parse::<SocketAddr>() {
            return Ok(addr);
        }
        if let Ok(ip) = s.parse::<std::net::IpAddr>() {
            return Ok(SocketAddr::new(ip, DEFAULT_PORT));
        }
        bail!("--target 需要 `IP` 或 `IP:端口`，收到 `{s}`")
    }

    fn flag(args: &[String], name: &str) -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    }

    fn number<T: std::str::FromStr>(s: &str, what: &str) -> Result<T>
    where
        T::Err: std::fmt::Display,
    {
        s.parse::<T>()
            .map_err(|e| anyhow::anyhow!("{what} 不是合法数字（`{s}`）：{e}"))
    }

    /// 采集块字节数（按 4 字节帧对齐）。
    fn aligned_chunk_bytes(chunk_ms: u32) -> usize {
        let raw = (BYTES_PER_SECOND * f64::from(chunk_ms) / 1000.0) as usize;
        raw - (raw % 4)
    }

    fn bytes_to_ms(bytes: u64) -> f64 {
        bytes as f64 / BYTES_PER_SECOND * 1000.0
    }

    fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
        v.map_or_else(|| "-".to_string(), |v| v.to_string())
    }

    /// 信号电平表：用来回答「到底推出去的是声音还是静音」。
    ///
    /// 0.12 FS 的测试音用户听不见（已踩过），所以每次运行都必须能看到峰值/RMS。
    #[derive(Default)]
    struct Meter {
        peak: u16,
        sum_sq: f64,
        samples: u64,
    }

    impl Meter {
        fn add(&mut self, pcm: &[u8]) {
            for b in pcm.chunks_exact(2) {
                let v = f64::from(i16::from_le_bytes([b[0], b[1]]));
                let a = v.abs();
                if a > f64::from(self.peak) {
                    self.peak = a.min(f64::from(i16::MAX)) as u16;
                }
                self.sum_sq += v * v;
                self.samples += 1;
            }
        }

        fn absorb(&mut self, other: &Self) {
            self.peak = self.peak.max(other.peak);
            self.sum_sq += other.sum_sq;
            self.samples += other.samples;
        }

        fn reset(&mut self) {
            *self = Self::default();
        }

        /// 峰值相对满量程的比例（1.0 = 削顶）。
        fn peak_fs(&self) -> f64 {
            f64::from(self.peak) / 32_768.0
        }

        /// RMS 相对满量程的比例。
        fn rms_fs(&self) -> f64 {
            if self.samples == 0 {
                return 0.0;
            }
            (self.sum_sq / self.samples as f64).sqrt() / 32_768.0
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn target_accepts_ip_and_ip_port() {
            assert_eq!(parse_target("192.168.1.21").unwrap().port(), 7000);
            assert_eq!(
                parse_target("192.168.1.21:8000").unwrap(),
                "192.168.1.21:8000".parse().unwrap()
            );
            assert!(parse_target("不是地址").is_err());
        }

        #[test]
        fn chunk_bytes_are_frame_aligned() {
            assert_eq!(aligned_chunk_bytes(10), 1_764); // 10 ms ≈ 441 帧
            assert_eq!(aligned_chunk_bytes(10) % 4, 0);
            assert_eq!(aligned_chunk_bytes(1), 176);
        }

        #[test]
        fn meter_reports_peak_and_rms_relative_to_full_scale() {
            let mut m = Meter::default();
            // 一个满量程样本 + 一个 0.5 量程样本
            m.add(&[0xFF, 0x7F, 0x00, 0x40]);
            assert!((m.peak_fs() - 1.0).abs() < 1e-3);
            assert!((m.rms_fs() - 0.79).abs() < 0.01, "{}", m.rms_fs());
            let empty = Meter::default();
            assert_eq!(empty.rms_fs(), 0.0);
        }

        #[test]
        fn bytes_to_ms_matches_44100_stereo_s16() {
            assert!((bytes_to_ms(176_400) - 1000.0).abs() < 1e-6);
            assert!((bytes_to_ms(1_764) - 10.0).abs() < 1e-6);
        }

        #[test]
        fn gain_one_is_bit_identical_and_zero_is_silence() {
            let src: Vec<u8> = (0..=255u8).collect();
            let mut buf = src.clone();
            apply_gain_s16le(&mut buf, 1.0).unwrap();
            assert_eq!(buf, src, "增益 1.0 必须逐字节不变");
            apply_gain_s16le(&mut buf, 0.0).unwrap();
            assert!(buf.iter().all(|b| *b == 0), "增益 0 必须得到纯静音");
        }

        #[test]
        fn gain_clamps_instead_of_wrapping() {
            // 满幅样本乘 2.0 若不夹取就会回绕成反相噪声——这是最容易听出来的 bug
            let mut buf = Vec::new();
            for s in [32_767i16, -32_768, 16_384, -16_384] {
                buf.extend_from_slice(&s.to_le_bytes());
            }
            apply_gain_s16le(&mut buf, 2.0).unwrap();
            let out: Vec<i16> = buf
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
            assert_eq!(out, vec![32_767, -32_768, 32_767, -32_768]);
        }

        #[test]
        fn gain_rejects_out_of_range_values() {
            let mut buf = [0u8; 4];
            assert!(apply_gain_s16le(&mut buf, f32::NAN).is_err());
            assert!(apply_gain_s16le(&mut buf, -0.1).is_err());
            assert!(apply_gain_s16le(&mut buf, miplay_airplay::MAX_GAIN + 0.1).is_err());
        }

        /// 采集 → 增益 → 分块 → 帧数换算：整条链路的算术自洽性（不需要真机）。
        #[test]
        fn capture_to_sender_pipeline_math_is_exact() {
            // 1 秒 44.1 kHz 立体声 s16 源：440 Hz 正弦，0.5 FS
            let frames = SAMPLE_RATE as usize;
            let mut src = Vec::with_capacity(frames * 4);
            for i in 0..frames {
                let phase =
                    2.0 * std::f64::consts::PI * 440.0 * i as f64 / f64::from(SAMPLE_RATE);
                let v = (0.5 * phase.sin() * 32_768.0) as i16;
                src.extend_from_slice(&v.to_le_bytes());
                src.extend_from_slice(&v.to_le_bytes());
            }
            assert_eq!(src.len(), BYTES_PER_SECOND as usize);

            // 按 10 ms 切块：必须刚好 100 块、每块帧对齐、合起来不丢不重
            let chunk = aligned_chunk_bytes(DEFAULT_CHUNK_MS);
            assert_eq!(chunk, 1_764);
            let chunks: Vec<&[u8]> = src.chunks_exact(chunk).collect();
            assert_eq!(chunks.len(), 100);
            assert_eq!(chunks.iter().map(|c| c.len()).sum::<usize>(), src.len());

            // 增益 0.5 后峰值从 0.5 FS 降到 0.25 FS、样本数不变（0.25 FS 正弦的 RMS = 0.1768）
            let mut total = Meter::default();
            for c in &chunks {
                let mut owned = c.to_vec();
                apply_gain_s16le(&mut owned, 0.5).unwrap();
                total.add(&owned);
            }
            assert_eq!(total.samples, u64::from(SAMPLE_RATE * u32::from(CHANNELS)));
            assert!((total.peak_fs() - 0.25).abs() < 0.01, "{}", total.peak_fs());
            assert!((total.rms_fs() - 0.177).abs() < 0.01, "{}", total.rms_fs());
        }
    }
}
