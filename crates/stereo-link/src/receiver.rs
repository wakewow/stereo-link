//! 手机投放接收端：把本机声卡变成 AirPlay / 小米妙播的投放目标。
//!
//! 两个接收功能跑在**独立进程**里（`airplay-recv.exe`、`miplay-recv.exe`），
//! 面板只负责拉起/停止/跟随默认声卡，不碰它们的音频数据：
//!
//! * 解耦：左右分声道引擎（[`crate::app`] 里的 router）完全不参与，手机声音
//!   直接从接收端写进 Windows 当前默认输出设备。
//! * 许可隔离：shairplay 是 LGPL-3.0-or-later、FusionPlay 妙播 SDK 是
//!   AGPL-3.0-only，都不能链接进以 MIT OR Apache-2.0 发布的 StereoLink 主程序；
//!   独立 exe 只通过命令行参数和标准输出交互，许可互不传染。
//! * 声卡跟随：两个接收端都用「系统默认输出」，这里在默认设备变化时重启它们，
//!   保证声音始终从**当前系统选中的那块声卡**出来。
//!
//! 设备名固定为 [`DEVICE_NAME`]，手机端在 AirPlay「隔空播放」/ 小米妙播里看到的
//! 就是这个名字。

use std::io::{BufRead, BufReader, Write};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Instant;

/// 手机端显示的设备名（用户要求固定为 StereoLink）。
pub const DEVICE_NAME: &str = "StereoLink";

/// 接收端程序相对 `stereo-link.exe` 的查找位置提示（界面上找不到 exe 时展示）。
pub const BUILD_HINT: &str =
    "在 receiver/ 目录执行：cargo build --release -p airplay-recv -p miplay-recv";

/// Windows 下不给子进程开控制台窗口（它们只靠标准输出回报状态）。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 接收端协议种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CastKind {
    /// 苹果 AirPlay（iPhone / iPad / Mac 的「隔空播放」）。
    AirPlay,
    /// 小米妙播（小米手机控制中心的「妙播」）。
    MiPlay,
}

impl CastKind {
    /// 界面上的展示顺序。
    pub const ALL: [CastKind; 2] = [CastKind::AirPlay, CastKind::MiPlay];

    /// 数组下标（`ReceiverManager::procs` 用）。
    fn index(self) -> usize {
        match self {
            CastKind::AirPlay => 0,
            CastKind::MiPlay => 1,
        }
    }

    /// 界面标题。
    pub fn label(self) -> &'static str {
        match self {
            CastKind::AirPlay => "AirPlay",
            CastKind::MiPlay => "小米妙播",
        }
    }

    /// 接收端可执行文件名。
    pub fn exe_name(self) -> &'static str {
        match self {
            CastKind::AirPlay => "airplay-recv.exe",
            CastKind::MiPlay => "miplay-recv.exe",
        }
    }

    /// 手机上怎么把声音投过来（界面提示用）。
    pub fn phone_hint(self) -> &'static str {
        match self {
            CastKind::AirPlay => {
                "iPhone / iPad：控制中心 → 音乐播放卡片右上角的「隔空播放」图标 → 选 StereoLink。\
                 \n首次连接（AirPlay 2）若手机要 4 位配对码，输入 3939；配对一次后快速重连、不再弹码。"
            }
            CastKind::MiPlay => {
                "小米手机：控制中心 →「小米妙播」→ 选 StereoLink（或音乐 App 里的投送按钮）。"
            }
        }
    }

    /// 这个接收端需要哪些命令行参数。
    ///
    /// 输出声卡一律交给接收端自己的「系统默认」：
    /// * `airplay-recv` 用 `--device default` 显式表达；
    /// * `miplay-recv` 不传 `--device` 就是默认设备，传了反而会先尝试名字匹配、失败再回退并打警告。
    ///
    /// `low_latency` 只影响小米妙播：上游 SDK 默认要攒够 0.65~0.8 秒才出声
    /// （为的是和小米「全屋播放」对齐），只投本机时明显慢半拍，这里可以把目标延迟压到 220 ms。
    fn args(self, low_latency: bool) -> &'static [&'static str] {
        match self {
            CastKind::AirPlay => &["--name", DEVICE_NAME, "--device", "default"],
            // `--quiet`：妙播 SDK 每秒都会吐发现/探测事件，控制台保留、面板里只留关键行。
            CastKind::MiPlay if low_latency => &[
                "--name",
                DEVICE_NAME,
                "--quiet",
                "--group-delay-ms",
                "220",
                "--min-queue-ms",
                "160",
            ],
            CastKind::MiPlay => &["--name", DEVICE_NAME, "--quiet"],
        }
    }
}

/// 接收端运行状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CastStatus {
    /// 没在跑。
    Stopped,
    /// 子进程在跑。
    Running,
    /// 启动失败或异常退出（附原因）。
    Failed(String),
}

impl CastStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, CastStatus::Running)
    }

    /// 状态胶囊的文案。
    pub fn text(&self) -> &'static str {
        match self {
            CastStatus::Stopped => "未运行",
            CastStatus::Running => "运行中",
            CastStatus::Failed(_) => "异常",
        }
    }

    /// 失败原因（只有 `Failed` 有）。
    pub fn error(&self) -> Option<&str> {
        match self {
            CastStatus::Failed(message) => Some(message),
            _ => None,
        }
    }
}

/// 一个接收端子进程。
struct Proc {
    kind: CastKind,
    /// 解析到的 exe 路径（找不到就是 `None`，界面据此提示去编译）。
    exe: Option<PathBuf>,
    child: Option<Child>,
    /// 子进程标准输入：妙播接收端从它读 `volume N` 控制行（AirPlay 用不上，但管道同样留住）。
    stdin: Option<ChildStdin>,
    status: CastStatus,
    /// 子进程最后一行输出（异常退出时拼进错误说明）。
    last_line: String,
    /// 接收端启动时报告的「输出声卡」名字（面板用它告诉用户声音到底出在哪）。
    device: Option<String>,
    /// 接收端自报的当前音量（0–100）；启动横幅和手机/主机的音量事件都会更新它。
    volume: Option<u8>,
}

/// 子进程输出行的发送端类型：`(接收端下标, 文本行)`。
type LogLine = (usize, String);

/// 两个接收端的生命周期管理。
pub struct ReceiverManager {
    procs: [Proc; 2],
    log_rx: Receiver<LogLine>,
    log_tx: Sender<LogLine>,
    /// 上一次看到的 Windows 默认输出端点 `(id, 名字)`；变化时重启运行中的接收端。
    ///
    /// 用 id 判断变化：WASAPI 端点经常好几个都叫「扬声器」，只看名字会漏掉切换。
    last_default: Option<(String, String)>,
    /// 最近一次「跟随默认声卡」重启的时间（日志里区分是用户点按钮还是自动重启）。
    last_follow: Option<Instant>,
    /// 妙播是否用低延迟参数启动（见 [`CastKind::args`]）。
    low_latency: bool,
}

impl Default for ReceiverManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ReceiverManager {
    pub fn new() -> Self {
        let (log_tx, log_rx) = mpsc::channel();
        let mut manager = Self {
            procs: [
                Proc {
                    kind: CastKind::AirPlay,
                    exe: None,
                    child: None,
                    stdin: None,
                    status: CastStatus::Stopped,
                    last_line: String::new(),
                    device: None,
                    volume: None,
                },
                Proc {
                    kind: CastKind::MiPlay,
                    exe: None,
                    child: None,
                    stdin: None,
                    status: CastStatus::Stopped,
                    last_line: String::new(),
                    device: None,
                    volume: None,
                },
            ],
            log_rx,
            log_tx,
            last_default: None,
            last_follow: None,
            low_latency: false,
        };
        // exe 路径只在第一次需要时解析（启动时解析一遍，界面就能直接显示「已找到/缺失」）。
        for proc in &mut manager.procs {
            proc.exe = locate(proc.kind);
        }
        manager
    }

    /// 当前状态。
    pub fn status(&self, kind: CastKind) -> &CastStatus {
        &self.procs[kind.index()].status
    }

    /// 解析到的 exe 路径（`None` = 这台机器上还没有编译接收端）。
    pub fn exe(&self, kind: CastKind) -> Option<&Path> {
        self.procs[kind.index()].exe.as_deref()
    }

    /// 正在跑的接收端报告的「输出声卡」名字（多开时取第一个有回报的）。
    pub fn output_device(&self) -> Option<&str> {
        self.procs
            .iter()
            .filter(|proc| proc.child.is_some())
            .find_map(|proc| proc.device.as_deref())
    }

    /// 是否有任意一个接收端在跑。
    pub fn is_any_running(&self) -> bool {
        self.procs.iter().any(|proc| proc.child.is_some())
    }

    /// 正在跑的接收端。
    pub fn running(&self) -> Vec<CastKind> {
        self.procs
            .iter()
            .filter(|proc| proc.child.is_some())
            .map(|proc| proc.kind)
            .collect()
    }

    /// 接收端自报的当前音量（0–100）；还没收到横幅或事件时为 `None`。
    pub fn volume(&self, kind: CastKind) -> Option<u8> {
        self.procs[kind.index()].volume
    }

    /// 设置妙播接收端的音量（0–100）。
    ///
    /// 妙播接收端从 stdin 读控制行（契约：`volume <0-100>\n`），改的是接收端
    /// 自己的音量：手机端和主机端都能继续调，接收端会把最新值写进事件行。
    /// AirPlay 的音量协商在手机侧，这里统一返回错误；没在跑同样报错。
    pub fn set_volume(&mut self, kind: CastKind, percent: u8) -> Result<(), String> {
        if percent > 100 {
            return Err(format!("音量只支持 0–100，收到 {percent}"));
        }
        if kind != CastKind::MiPlay {
            return Err("AirPlay 的音量由手机端控制".to_string());
        }
        let proc = &mut self.procs[kind.index()];
        if proc.child.is_none() {
            return Err(format!("{} 接收端未运行", kind.label()));
        }
        let Some(stdin) = proc.stdin.as_mut() else {
            return Err(format!("{} 接收端没有 stdin 通道", kind.label()));
        };
        writeln!(stdin, "volume {percent}")
            .and_then(|()| stdin.flush())
            .map_err(|error| format!("写入接收端失败：{error}"))?;
        proc.volume = Some(percent);
        Ok(())
    }

    /// 妙播是否用低延迟参数启动；已经在跑就顺便重启，让新设置立刻生效。
    pub fn set_low_latency(&mut self, low_latency: bool) {
        if self.low_latency == low_latency {
            return;
        }
        self.low_latency = low_latency;
        if self.procs[CastKind::MiPlay.index()].child.is_some() {
            self.stop(CastKind::MiPlay);
            if let Err(error) = self.start(CastKind::MiPlay) {
                tracing::warn!("[投放] 低延迟设置切换后重启妙播失败：{error}");
            }
        }
    }

    /// 启动一个接收端。已经在跑就直接返回 `Ok`。
    pub fn start(&mut self, kind: CastKind) -> Result<(), String> {
        let index = kind.index();
        if self.procs[index].child.is_some() {
            return Ok(());
        }
        // 上次没找到，这次再找一遍：用户可能刚编译完。
        if self.procs[index].exe.is_none() {
            self.procs[index].exe = locate(kind);
        }
        let Some(exe) = self.procs[index].exe.clone() else {
            let message = format!("找不到 {}（{}）", kind.exe_name(), BUILD_HINT);
            tracing::warn!("[投放] {message}");
            self.procs[index].status = CastStatus::Failed(message.clone());
            return Err(message);
        };

        let mut command = Command::new(&exe);
        command
            .args(kind.args(self.low_latency))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        match command.spawn() {
            Ok(mut child) => {
                // stdin 留着：妙播接收端从它读 `volume N` 控制行（AirPlay 不读也无妨）。
                let stdin = child.stdin.take();
                // 两个管道各起一个读取线程，读到的每一行都送回 `poll()` 转进日志面板。
                if let Some(stdout) = child.stdout.take() {
                    let tx = self.log_tx.clone();
                    thread::spawn(move || forward_lines(stdout, index, tx));
                }
                if let Some(stderr) = child.stderr.take() {
                    let tx = self.log_tx.clone();
                    thread::spawn(move || forward_lines(stderr, index, tx));
                }
                tracing::info!(
                    "[投放] 已启动 {} 接收端（设备名 {DEVICE_NAME}，输出跟随系统默认声卡）",
                    kind.label()
                );
                self.procs[index].stdin = stdin;
                self.procs[index].child = Some(child);
                self.procs[index].status = CastStatus::Running;
                self.procs[index].last_line.clear();
                self.procs[index].device = None;
                self.procs[index].volume = None;
                Ok(())
            }
            Err(error) => {
                let message = format!("启动 {} 失败：{error}", kind.exe_name());
                tracing::warn!("[投放] {message}");
                self.procs[index].status = CastStatus::Failed(message.clone());
                self.procs[index].child = None;
                Err(message)
            }
        }
    }

    /// 停止一个接收端（没有在跑就什么都不做）。
    pub fn stop(&mut self, kind: CastKind) {
        let index = kind.index();
        // 先放掉 stdin（管道随之关闭），再杀进程，避免接收端还在等控制行。
        self.procs[index].stdin = None;
        self.procs[index].volume = None;
        if let Some(mut child) = self.procs[index].child.take() {
            let _ = child.kill();
            let _ = child.wait();
            tracing::info!("[投放] 已停止 {} 接收端", kind.label());
        }
        self.procs[index].status = CastStatus::Stopped;
        self.procs[index].device = None;
    }

    /// 切换开关：在跑就停、没跑就起。
    pub fn toggle(&mut self, kind: CastKind) -> Result<(), String> {
        if self.procs[kind.index()].child.is_some() {
            self.stop(kind);
            Ok(())
        } else {
            self.start(kind)
        }
    }

    /// 退出面板时全部停掉，避免留下占着 5000 端口的孤儿进程。
    pub fn stop_all(&mut self) {
        for kind in CastKind::ALL {
            self.stop(kind);
        }
    }

    /// 每帧调用：转发子进程输出、更新存活状态。
    pub fn poll(&mut self) {
        // 1) 子进程输出 → 日志面板（只抬关键行，细节留在接收端自己的控制台）。
        while let Ok((index, line)) = self.log_rx.try_recv() {
            let line = line.trim_end().to_string();
            if line.is_empty() {
                continue;
            }
            let Some(proc) = self.procs.get_mut(index) else {
                continue;
            };
            proc.last_line = line.clone();
            if let Some(device) = device_of(&line) {
                proc.device = Some(device);
            }
            if let Some(volume) = volume_of(&line) {
                proc.volume = Some(volume);
            }
            if is_interesting(&line) {
                tracing::info!("[{}] {}", proc.kind.label(), clip(&line, 300));
            }
        }

        // 2) 进程是否还活着。
        for proc in &mut self.procs {
            let Some(child) = proc.child.as_mut() else {
                continue;
            };
            match child.try_wait() {
                Ok(None) => {
                    proc.status = CastStatus::Running;
                }
                Ok(Some(exit)) => {
                    let code = match exit.code() {
                        Some(code) => format!("退出码 {code}"),
                        None => "被系统终止".to_string(),
                    };
                    let message = if proc.last_line.is_empty() {
                        format!("{} 已退出（{code}）", proc.kind.exe_name())
                    } else {
                        format!(
                            "{} 已退出（{code}），最后输出：{}",
                            proc.kind.exe_name(),
                            clip(&proc.last_line, 160)
                        )
                    };
                    tracing::warn!("[投放] {message}");
                    proc.child = None;
                    proc.stdin = None;
                    proc.device = None;
                    proc.volume = None;
                    proc.status = CastStatus::Failed(message);
                }
                Err(error) => {
                    tracing::warn!("[投放] 无法获取 {} 运行状态：{error}", proc.kind.exe_name());
                }
            }
        }
    }

    /// 跟随系统默认输出：默认声卡变了就把在跑的接收端重启一遍。
    ///
    /// 接收端在启动时打开当时那块声卡，之后不会自己换设备；要跟着 Windows 的
    /// 「默认播放设备」走，只能重开。StereoLink 自己切默认输出（开始路由时把默认
    /// 输出换成采集端点）也会触发这里，于是手机声音自然跟着系统走。
    ///
    /// `current` 传默认输出端点的 `(id, 名字)`；只有 id 变了才算切换。
    pub fn sync_default_output(&mut self, current: Option<(&str, &str)>) {
        let Some((id, name)) = current else {
            return;
        };
        if id.trim().is_empty() {
            return;
        }
        if self
            .last_default
            .as_ref()
            .is_some_and(|(last, _)| last == id)
        {
            return;
        }
        let previous = self
            .last_default
            .replace((id.to_string(), name.to_string()));
        // 第一次拿到默认设备只是初始化，不重启。
        let Some((_, previous_name)) = previous else {
            return;
        };
        let running = self.running();
        if running.is_empty() {
            return;
        }
        tracing::info!(
            "[投放] 系统默认输出从「{}」变成「{}」，重启运行中的接收端以跟随",
            clip(&previous_name, 60),
            clip(name, 60)
        );
        self.last_follow = Some(Instant::now());
        for kind in running {
            self.stop(kind);
            let _ = self.start(kind);
        }
    }

    /// 最近一次「跟随默认声卡」重启的时间（留给界面显示，避免重复解释）。
    pub fn last_follow(&self) -> Option<Instant> {
        self.last_follow
    }
}

/// 把子进程管道的每一行送到主线程。
fn forward_lines<R: std::io::Read + Send + 'static>(pipe: R, index: usize, tx: Sender<LogLine>) {
    for line in BufReader::new(pipe).lines() {
        match line {
            Ok(line) => {
                if tx.send((index, line)).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// 从接收端输出里认出「输出声卡」那一行，取出设备名。
///
/// 两个接收端的行格式不同：
/// * `airplay-recv`：`🔈 输出声卡：扬声器（44100 Hz / 2 声道）`
/// * `miplay-recv` ：`🔈 输出声卡：扬声器｜48000 Hz / 2 ch / F32｜id=wasapi:…（系统默认）`
fn device_of(line: &str) -> Option<String> {
    let rest = line.split_once("输出声卡")?.1;
    let rest = rest.trim_start_matches(['：', ':', ' ']).trim();
    if rest.is_empty() {
        return None;
    }
    // 先切掉 `｜格式｜id` 尾巴，再去掉 `（…）` 括号里的采样率说明。
    let head = rest.split('｜').next().unwrap_or(rest).trim();
    let name = head.split('（').next().unwrap_or(head).trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// 从接收端输出里认出当前音量：启动横幅和手机/主机改音量的 JSON 事件都算。
///
/// 契约 A 的两种行：
/// * 启动横幅：`🔊 接收端音量：50（手机端可调，面板/CLI 也能改）`
/// * 事件行 ：`[妙播] volume {"source":"host","percent":50}`
///
/// 越界的数字一律夹到 0–100，接收端打错也不让界面拿到怪值。
fn volume_of(line: &str) -> Option<u8> {
    if let Some((_, rest)) = line.split_once("接收端音量") {
        let rest = rest.trim_start_matches(['：', ':', ' ']);
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(volume) = digits.parse::<u32>() {
            return Some(volume.min(100) as u8);
        }
    }
    let (_, rest) = line.split_once("volume")?;
    let value: serde_json::Value = serde_json::from_str(rest.trim()).ok()?;
    let percent = value.get("percent")?;
    let percent = percent
        .as_i64()
        .or_else(|| percent.as_f64().map(|value| value.round() as i64))?;
    Some(percent.clamp(0, 100) as u8)
}

/// 哪些输出行值得抬进面板日志。
/// 接收端自己会打发现/探测类明细（每秒数条），只在它们的控制台留着；这里保留
/// 就绪、播放、设备、错误这类对用户有意义的行。
fn is_interesting(line: &str) -> bool {
    const NOISE: [&str; 9] = [
        "discovery_",
        "audio_pcm",
        "packet_received",
        "clock_sync",
        "\"seq\"",
        "peer_query",
        "advertisement",
        "generic_tv_discovery",
        // 音量事件：拖动面板滑杆时每帧一条，界面滑杆已经显示，日志里不刷屏。
        "volume",
    ];
    !NOISE.iter().any(|noise| line.contains(noise))
}

fn clip(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let clipped: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{clipped}…")
    } else {
        clipped
    }
}

/// 清掉上次运行残留的接收端进程。
///
/// GUI 被强杀（任务管理器 / `Stop-Process -Force`）时子进程不会跟着退出，
/// 5000/8899 端口被占后新的接收端起不来（表现为「端口只允许使用一次」）。
/// 启动时扫一遍同名进程并结束掉；手动调试接收端时设
/// `STEREOLINK_KEEP_STALE_RECEIVERS=1` 可跳过这一步。
pub fn kill_stale() {
    if std::env::var_os("STEREOLINK_KEEP_STALE_RECEIVERS").is_some() {
        return;
    }
    for kind in CastKind::ALL {
        let name = kind.exe_name();
        let pids = stale_pids(name);
        if pids.is_empty() {
            continue;
        }
        tracing::warn!("发现上次运行的 {name} 残留（pid {pids:?}），先结束它们");
        let mut command = Command::new("taskkill");
        command.args(["/F", "/T"]);
        for pid in &pids {
            command.args(["/PID", &pid.to_string()]);
        }
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        let _ = command.status();
    }
}

/// 查同名进程的 pid（`tasklist` 输出解析；查不到返回空）。
fn stale_pids(exe: &str) -> Vec<u32> {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {exe}"), "/FO", "CSV", "/NH"])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split("\",\"");
            let name = parts.next()?.trim_start_matches('"');
            if !name.eq_ignore_ascii_case(exe) {
                return None;
            }
            parts.next()?.parse().ok()
        })
        .collect()
}
/// 找到接收端可执行文件。
///
/// 查找顺序（先命中先用）：
/// 1. 环境变量 `STEREOLINK_RECEIVER_DIR`（便携包/自定义安装位置）；
/// 2. 主程序所在目录、以及它下面的 `receivers/`（正式打包形态）；
/// 3. 从当前 exe 逐级向上回溯工作区根，找 `receiver/target/{release,debug}`
///    （开发期 `cargo run` / `cargo build` / `cargo test` 形态，从任意目录启动都能命中）。
pub fn locate(kind: CastKind) -> Option<PathBuf> {
    let name = kind.exe_name();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("STEREOLINK_RECEIVER_DIR") {
        dirs.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
            dirs.push(dir.join("receivers"));
        }
    }
    if let Some(path) = dirs
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
    {
        return Some(path);
    }
    locate_in_workspace(name)
}

/// 从当前 exe 向上回溯，找开发期的 `receiver/target/{release,debug}`。
///
/// 不用 `env!("CARGO_MANIFEST_DIR")`：那会把构建机的绝对路径编进 exe（发布产物
/// 不该泄露用户名/目录，见 `scripts/build-release.ps1` 的自检）。
fn locate_in_workspace(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?.to_path_buf();
    for _ in 0..5 {
        for profile in ["release", "debug"] {
            let candidate = dir.join("receiver").join("target").join(profile).join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_always_follow_the_system_default_device() {
        // AirPlay 显式写 default；妙播不写 --device 就是默认设备。
        assert!(CastKind::AirPlay.args(false).contains(&"default"));
        assert!(!CastKind::MiPlay.args(false).contains(&"--device"));
    }

    #[test]
    fn args_use_the_fixed_device_name() {
        for kind in CastKind::ALL {
            for low_latency in [false, true] {
                let args = kind.args(low_latency);
                let index = args
                    .iter()
                    .position(|arg| *arg == "--name")
                    .expect("要传设备名");
                assert_eq!(args[index + 1], DEVICE_NAME);
            }
        }
    }

    #[test]
    fn low_latency_only_changes_miplay() {
        assert_eq!(CastKind::AirPlay.args(false), CastKind::AirPlay.args(true));
        let args = CastKind::MiPlay.args(true);
        assert!(args.contains(&"--group-delay-ms") && args.contains(&"220"));
        assert!(args.contains(&"--min-queue-ms") && args.contains(&"160"));
        assert!(!CastKind::MiPlay.args(false).contains(&"--group-delay-ms"));
    }

    #[test]
    fn airplay_hint_mentions_pairing_pin() {
        // AirPlay 2 首次连接会要 4 位配对码，面板提示必须写出来，否则用户不知道输什么。
        assert!(CastKind::AirPlay.phone_hint().contains("3939"));
        assert!(!CastKind::MiPlay.phone_hint().contains("3939"));
    }

    #[test]
    fn detects_output_device_from_receiver_lines() {
        // airplay-recv 的行
        assert_eq!(
            device_of("🔈 输出声卡：扬声器（44100 Hz / 2 声道）").as_deref(),
            Some("扬声器")
        );
        // miplay-recv 的行
        assert_eq!(
            device_of(
                "🔈 输出声卡：扬声器｜48000 Hz / 2 ch / F32｜id=wasapi:{0.0.0.00000000}.{abc}（系统默认）"
            )
            .as_deref(),
            Some("扬声器")
        );
        assert_eq!(device_of("✅ 已就绪"), None);
    }

    #[test]
    fn detects_volume_from_banner_and_events() {
        // 启动横幅：全角 / 半角冒号都要认。
        assert_eq!(
            volume_of("🔊 接收端音量：50（手机端可调，面板/CLI 也能改）"),
            Some(50)
        );
        assert_eq!(volume_of("🔊 接收端音量: 7"), Some(7));
        // 手机端改音量 / 主机（面板/CLI）改音量的事件行。
        assert_eq!(
            volume_of("[妙播] volume {\"source\":\"xiaomi\",\"percent\":30}"),
            Some(30)
        );
        assert_eq!(
            volume_of("[妙播] volume {\"source\":\"host\",\"percent\":100}"),
            Some(100)
        );
        // 越界夹到 0–100；坏 JSON / 无关行返回 None。
        assert_eq!(volume_of("[妙播] volume {\"percent\":150}"), Some(100));
        assert_eq!(volume_of("[妙播] volume {\"percent\":-5}"), Some(0));
        assert_eq!(volume_of("🔊 接收端音量：250"), Some(100));
        assert_eq!(volume_of("[妙播] volume 不是 JSON"), None);
        assert_eq!(volume_of("✅ 已就绪"), None);
        assert_eq!(device_of("🔊 接收端音量：50"), None);
    }

    #[test]
    fn set_volume_rejects_airplay_and_stopped_receiver() {
        let mut cast = ReceiverManager::new();
        let airplay = cast.set_volume(CastKind::AirPlay, 30).unwrap_err();
        assert!(airplay.contains("手机端"), "{airplay}");
        let stopped = cast.set_volume(CastKind::MiPlay, 30).unwrap_err();
        assert!(stopped.contains("未运行"), "{stopped}");
        let too_loud = cast.set_volume(CastKind::MiPlay, 150).unwrap_err();
        assert!(too_loud.contains("0–100"), "{too_loud}");
        // 失败不影响已知音量。
        assert_eq!(cast.volume(CastKind::MiPlay), None);
    }

    #[test]
    fn noise_lines_are_filtered_out() {
        assert!(!is_interesting(
            "discovery_packet_received source=192.168.1.21"
        ));
        assert!(!is_interesting(
            "{\"event\":\"audio_pcm_decoded\",\"frames\":1024}"
        ));
        assert!(is_interesting("✅ 小米妙播接收端已就绪：「StereoLink」"));
        assert!(is_interesting("🔈 输出声卡：扬声器"));
        // 音量事件很密（面板拖动滑杆时每帧一条），只更新状态不进日志。
        assert!(!is_interesting(
            "[妙播] volume {\"source\":\"host\",\"percent\":30}"
        ));
        assert!(is_interesting(
            "🔊 接收端音量：50（手机端可调，面板/CLI 也能改）"
        ));
    }

    #[test]
    fn workspace_lookup_skips_unknown_binaries() {
        // 定位走运行期的路径回溯，不依赖编译期 CARGO_MANIFEST_DIR
        // （否则构建机的绝对路径会被编进发布产物）。
        assert!(locate_in_workspace("stereolink-no-such-recv-9f3c.exe").is_none());
    }

    #[test]
    fn clip_keeps_short_text_and_marks_long_text() {
        assert_eq!(clip("abc", 5), "abc");
        assert_eq!(clip("abcdef", 3), "abc…");
    }

    #[test]
    fn status_texts_are_stable() {
        assert_eq!(CastStatus::Stopped.text(), "未运行");
        assert_eq!(CastStatus::Running.text(), "运行中");
        assert_eq!(CastStatus::Failed("x".into()).text(), "异常");
        assert!(CastStatus::Running.is_running());
    }
}
