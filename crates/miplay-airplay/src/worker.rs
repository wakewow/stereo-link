//! 协议后端：把 `cidermill-airplay` 的**同步阻塞** API 包成一条专用工作线程。
//!
//! ⚠️ 这是本 crate 唯一依赖 GPL-3.0-or-later 库的文件。若要换成自研实现或别的库，
//! 只需要替换本文件，[`crate::AirPlaySender`] 的公开 API 不受影响。
//!
//! 线程内的结构（每个循环一次）：
//! 1. 推进 `AudioSendState` 状态机（把 `Action::Send` 的 RTSP 请求发出去）；
//! 2. 收取外部命令（PCM / 音量 / 停止）；
//! 3. 按「到期 + 提前两个包」的节奏发 RTP 音频包，每秒补一个同步包（声明延迟）。

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cidermill_airplay::audio::{
    Action, AudioSendConfig, AudioSendState, StreamType, TimingMode,
};
use cidermill_airplay::pairing::TransientStyle;
use cidermill_airplay::{Auth, ControlChannel, NtpTimestamp, OsRng};
use tokio::sync::{mpsc, oneshot, watch};

use crate::pcm::{PacketChunker, PcmFormat};
use crate::{
    Counter, Counters, Error, MAX_QUEUED_MS, Result, SEND_AHEAD_PACKETS, SenderConfig, SessionInfo,
    Status,
};

/// 外部发给工作线程的命令。
#[derive(Debug)]
pub(crate) enum Command {
    /// 一块 `s16le` 交错 PCM。
    Pcm(Vec<u8>),
    /// 音量（dB）。
    Volume(f64),
    /// 停止并收尾（发 TEARDOWN）。
    Stop,
}

/// 收到 `Stop` 后，最多再等这么久等 `TEARDOWN` 收尾，避免 `stop()` 永久挂住。
const STOP_GRACE: Duration = Duration::from_secs(3);
/// 没有音频可发时的休眠，避免空转烧 CPU。
const IDLE_SLEEP: Duration = Duration::from_millis(5);
/// 单次循环最多补发的包数（防止长时间挂起后一次性洪水）。
const MAX_PACKETS_PER_TICK: u64 = 64;
/// 单次循环最多处理的命令数。
const MAX_COMMANDS_PER_TICK: usize = 64;

/// 工作线程入口：建立会话、推流、收尾。
pub(crate) fn run(
    target: SocketAddr,
    cfg: SenderConfig,
    mut cmd: mpsc::Receiver<Command>,
    ready: oneshot::Sender<Result<SessionInfo>>,
    status: watch::Sender<Status>,
    counters: Arc<Counters>,
) {
    let mut ready = Some(ready);
    let outcome = session(target, &cfg, &mut cmd, &mut ready, &status, &counters);
    let failure = match outcome {
        Ok(()) => {
            let _ = status.send(Status::Stopped);
            None
        }
        Err(err) => {
            let _ = status.send(Status::Failed(err.to_string()));
            Some(err)
        }
    };
    if let Some(tx) = ready.take() {
        // 还没进入 Streaming 就结束了：把原因回报给 connect()
        let msg = failure.map_or_else(
            || "会话在进入推流状态前结束".to_string(),
            |err| err.to_string(),
        );
        let _ = tx.send(Err(Error::Session(msg)));
    }
}

/// 记一条带相对时间的时间线（毫秒）。
///
/// 设备在某些情况下会**收下请求但不回包**（我们实测过：请求已写出、10 秒读超时），
/// 这时"进行到哪一步"是唯一的线索，所以要先把动作记下来、再发请求。
fn note(journal: &mut Vec<String>, since: Instant, what: impl AsRef<str>) {
    journal.push(format!("+{:>5}ms {}", since.elapsed().as_millis(), what.as_ref()));
}

/// 把整条时间线拼成一行，附在错误后面。
fn timeline(journal: &[String]) -> String {
    journal.join(" | ")
}

fn session(
    target: SocketAddr,
    cfg: &SenderConfig,
    cmd: &mut mpsc::Receiver<Command>,
    ready: &mut Option<oneshot::Sender<Result<SessionInfo>>>,
    status_tx: &watch::Sender<Status>,
    counters: &Arc<Counters>,
) -> Result<()> {
    let host = target.ip().to_string();
    let format = cfg.codec.stream_format();
    let clock = Instant::now();
    let mut journal: Vec<String> = Vec::new();

    // 1) 控制通道 + HAP 瞬时配对（PIN 3939，不需要用户输入，也不落盘任何凭据）
    note(&mut journal, clock, format!("连接 {}:{}", host, target.port()));
    let mut channel = ControlChannel::builder(&host)
        .port(target.port())
        .connect()
        .map_err(|err| {
            Error::Session(format!("控制通道连接失败：{err}（时间线：{}）", timeline(&journal)))
        })?;
    // 1b) 设备自述（GET /info）。参照客户端（pyatv 0.18）在这台设备上的第一个请求就是它，
    //     我们照做：既让请求顺序与已验证的会话一致，也顺手拿到设备自己报的能力与音量模型。
    match channel.device_info() {
        Ok(info) => note(
            &mut journal,
            clock,
            format!(
                "设备自述：{} / {} / srcvers {} / 设备看到的我方地址 {} / 初始音量 {:?} / 音量模型 {:?} / 低延迟音频流 {:?} / 缓冲流 {:?}",
                info.name.as_deref().unwrap_or("-"),
                info.model.as_deref().unwrap_or("-"),
                info.source_version.as_deref().unwrap_or("-"),
                info.sender_address.as_deref().unwrap_or("-"),
                info.initial_volume,
                info.volume_control_type,
                info.supported_formats.low_latency_audio_stream,
                info.supported_formats.buffer_stream,
            ),
        ),
        Err(err) => note(&mut journal, clock, format!("设备自述读取失败（继续）：{err}")),
    }

    note(&mut journal, clock, "控制通道已连接，开始 HAP 瞬时配对");
    channel
        .authenticate(Auth::Transient(TransientStyle::Ios26), &mut OsRng)
        .map_err(|err| {
            Error::Session(format!("HAP 配对失败：{err}（时间线：{}）", timeline(&journal)))
        })?;
    let paired = channel.connection().rtsp_session_id();
    note(&mut journal, clock, format!("HAP 配对完成（session id {paired}）"));

    // 2) 时序服务 + 事件通道（MediaSession 必须活到会话结束）
    note(&mut journal, clock, "发出会话 SETUP（时序 + 事件通道）");
    let _media = channel.start_media_session().map_err(|err| {
        Error::Session(format!(
            "会话 SETUP 失败（这是本连接的第 1 个 RTSP 请求）：{err}（时间线：{}）",
            timeline(&journal)
        ))
    })?;
    note(&mut journal, clock, "会话 SETUP 成功，事件通道已打开");

    // 3) 两个本地 UDP 口：音频 RTP 与 控制 RTP（同步包从这里发）
    let local = channel.connection().local_ip();
    let data = UdpSocket::bind((local, 0))?;
    let control = UdpSocket::bind((local, 0))?;

    // 4) 实时流配置：把我们的控制口告诉设备（重传/时序探测会打到这里）
    let config = AudioSendConfig::generate(
        channel.connection(),
        StreamType::Realtime,
        format,
        control.local_addr()?.port(),
        TimingMode::Ntp,
        &mut OsRng,
    );
    let mut state = AudioSendState::new(config);
    let mut packetizer = state.packetizer()?;

    let pcm_format = PcmFormat::S16LE_44100_STEREO;
    let packet_frames = format.frames_per_packet;
    let packet_seconds = f64::from(packet_frames) / f64::from(format.sample_rate);
    let max_queue_bytes =
        pcm_format.bytes_for_frames((pcm_format.sample_rate as usize * MAX_QUEUED_MS) / 1000);
    let mut chunker = PacketChunker::new(pcm_format, packet_frames);

    let mut streaming: Option<Instant> = None;
    let mut syncs: u64 = 0;
    let mut stop_requested = false;
    let mut stop_deadline: Option<Instant> = None;
    let mut info = SessionInfo::default();

    loop {
        // --- 1. 状态机 ---
        match state.poll(clock.elapsed()) {
            Action::Send { step, request } => {
                note(
                    &mut journal,
                    clock,
                    format!("发出 {step:?}（{} 字节 body）", request.body.len()),
                );
                let response = channel.session_request(request).map_err(|err| {
                    Error::Session(format!(
                        "{step:?} 收不到响应：{err}（时间线：{}）",
                        timeline(&journal)
                    ))
                })?;
                note(
                    &mut journal,
                    clock,
                    format!("{step:?} -> {} {}", response.status, response.reason),
                );
                info.steps
                    .push(format!("{step:?} -> {} {}", response.status, response.reason));
                state.on_response(&response).map_err(|err| {
                    Error::Session(format!(
                        "{step:?} 的响应无法处理：{err}（时间线：{}）",
                        timeline(&journal)
                    ))
                })?;
            }
            Action::ConnectData {
                data_port,
                control_port,
                ..
            } => {
                note(
                    &mut journal,
                    clock,
                    format!("连接数据口 {data_port} / 控制口 {control_port}"),
                );
                data.connect((target.ip(), data_port))?;
                control.connect((target.ip(), control_port))?;
                info.data_port = Some(data_port);
                info.control_port = Some(control_port);
                state.data_connected();
            }
            Action::StartStreaming { .. } => {
                if let Some(stream) = state.stream() {
                    info.data_port = Some(stream.data_port);
                    info.control_port = Some(stream.control_port);
                    info.stream_id = stream.stream_id;
                    info.audio_buffer_size = stream.audio_buffer_size;
                    // 设备自报的内部渲染延迟：延迟预算里最有价值的一项
                    info.arrival_to_render_latency_ms = stream.arrival_to_render_latency_ms;
                }
                streaming = Some(Instant::now());
                let _ = status_tx.send(Status::Streaming);
                if let Some(tx) = ready.take() {
                    let _ = tx.send(Ok(info.clone()));
                }
            }
            Action::Finished => return Ok(()),
            Action::Failed {
                step,
                status,
                reason,
            } => {
                return Err(Error::Session(format!(
                    "{step:?} 失败：status={status:?} reason={reason}"
                )));
            }
            // `Wait`、PTP 专用的 `NeedAnchor`（我们走 NTP），以及将来新增的变体
            _ => {}
        }

        // --- 2. 命令 ---
        for _ in 0..MAX_COMMANDS_PER_TICK {
            match cmd.try_recv() {
                Ok(Command::Pcm(bytes)) => {
                    if !stop_requested {
                        chunker.push(&bytes);
                        let dropped = chunker.trim_to(max_queue_bytes);
                        if dropped > 0 {
                            counters.add(Counter::Dropped, dropped as u64);
                        }
                    }
                }
                Ok(Command::Volume(db)) => {
                    if !stop_requested {
                        state.set_volume(db);
                    }
                }
                Ok(Command::Stop) => stop_requested = true,
                Err(mpsc::error::TryRecvError::Empty) => break,
                // 句柄被丢（调用方没显式 stop）＝ 收尾
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    stop_requested = true;
                    break;
                }
            }
        }

        // --- 3. 停止 ---
        if stop_requested && streaming.is_some() {
            state.stop();
            streaming = None;
            stop_deadline = Some(Instant::now() + STOP_GRACE);
        }
        if let Some(deadline) = stop_deadline {
            if Instant::now() >= deadline {
                // TEARDOWN 没等到也走人，不能把 stop() 永久挂住
                return Ok(());
            }
        }

        counters.set(Counter::Queued, chunker.queued_bytes() as u64);

        // --- 4. 发包 ---
        let Some(started) = streaming else {
            std::thread::sleep(IDLE_SLEEP);
            continue;
        };
        let elapsed = started.elapsed().as_secs_f64();

        // 同步包：每秒一个，`first` 标记第一个；这里声明我们的延迟
        if elapsed.floor() as u64 >= syncs {
            let sync = packetizer.sync_packet(syncs == 0, cfg.latency_frames, NtpTimestamp::now());
            control.send(&sync.encode())?;
            syncs += 1;
            counters.add(Counter::Syncs, 1);
        }

        // 到期的包就发；提前 SEND_AHEAD_PACKETS 个包发，抹平 Windows 15.6 ms 定时器抖动
        let horizon = elapsed + SEND_AHEAD_PACKETS as f64 * packet_seconds;
        let mut sent_now = 0u64;
        while sent_now < MAX_PACKETS_PER_TICK
            && (counters.value(Counter::Packets) as f64) * packet_seconds <= horizon
        {
            let Some((samples, underrun)) = chunker.take_packet_padded() else {
                break; // 采集端还没喂数据
            };
            if underrun {
                counters.add(Counter::Underruns, 1);
            }
            let packet = packetizer.packet(&samples)?;
            let payload_len = packet.len();
            data.send(&packet)?;
            counters.add(Counter::Packets, 1);
            counters.add(Counter::Frames, u64::from(packet_frames));
            counters.add(Counter::Bytes, payload_len as u64);
            sent_now += 1;
        }

        // 睡到下一个包到期（提前量已经发满时才睡）
        let packets = counters.value(Counter::Packets);
        let target_time = (packets as f64 - SEND_AHEAD_PACKETS as f64).max(0.0) * packet_seconds;
        let remaining = target_time - started.elapsed().as_secs_f64();
        if remaining > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(remaining.min(0.05)));
        } else if sent_now == 0 {
            std::thread::sleep(IDLE_SLEEP);
        }
    }
}
