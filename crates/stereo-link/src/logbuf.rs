//! 日志出口：内存环形缓冲（GUI 面板 / `status` 里的 `log`）+ 落盘文件。
//!
//! GUI 进程是 `windows_subsystem = "windows"`（不弹黑框），所以日志面板是界面上的诊断
//! 出口；**同时**每个实例都把日志追加写到 `%APPDATA%\stereo-link\logs\stereo-link.log`，
//! 这样 `stereo-link-cli log` 在实例没跑的时候也能看到上一次运行的尾巴——这是开发规范
//! 「日志必须能从命令行拿到」的底层保证。

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// 一行日志。
#[derive(Debug, Clone)]
pub struct LogLine {
    /// 级别（INFO / WARN / ERROR ...）。
    pub level: Level,
    /// 正文。
    pub text: String,
    /// 距程序启动多久。
    pub at: Duration,
}

#[derive(Debug)]
struct Inner {
    lines: VecDeque<LogLine>,
    cap: usize,
    start: Instant,
}

/// 可克隆的环形日志缓冲（Layer 写、GUI 读共用同一份）。
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<Inner>>,
}

impl LogBuffer {
    /// `cap` 是保留的最大行数（超了丢最旧的）。
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                lines: VecDeque::with_capacity(cap.min(64)),
                cap: cap.max(16),
                start: Instant::now(),
            })),
        }
    }

    /// 追加一行。
    pub fn push(&self, level: Level, text: String) {
        let Ok(mut g) = self.inner.lock() else {
            return;
        };
        let at = g.start.elapsed();
        if g.lines.len() >= g.cap {
            g.lines.pop_front();
        }
        g.lines.push_back(LogLine { level, text, at });
    }

    /// 取一份快照（克隆；GUI 每帧调用）。
    pub fn lines(&self) -> Vec<LogLine> {
        match self.inner.lock() {
            Ok(g) => g.lines.iter().cloned().collect(),
            Err(poisoned) => poisoned.into_inner().lines.iter().cloned().collect(),
        }
    }

    /// 行数。
    pub fn len(&self) -> usize {
        match self.inner.lock() {
            Ok(g) => g.lines.len(),
            Err(poisoned) => poisoned.into_inner().lines.len(),
        }
    }

    /// 是否为空（clippy 要求 `len` 有配套的 `is_empty`）。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 清空。
    pub fn clear(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.lines.clear();
        }
    }
}

/// 把日志缓冲接到全局 `tracing` 订阅器上。
///
/// 三层同时装（任意一层失败只丢那一层，不影响启动）：
/// * 内存环形缓冲（GUI 面板 / 控制面 `log` 命令）；
/// * 文件 `%APPDATA%\stereo-link\logs\stereo-link.log`（超过 2 MB 轮转到 `.1`）；
/// * 控制台——只有设了 `STEREOLINK_LOG_CONSOLE=1` 才开（GUI 正式版没有控制台；
///   `serve` / 命令行子命令会自己设上）。
pub fn install(buffer: LogBuffer) {
    use tracing_subscriber::layer::SubscriberExt;

    let file_layer = FileLayer::open().map_err(|error| {
        // 这里还不能用 tracing（订阅器没装），只能吞掉；doctor 会检查日志目录。
        eprintln!("日志文件不可用：{error}");
    });
    let console = std::env::var_os("STEREOLINK_LOG_CONSOLE").is_some();

    let registry = tracing_subscriber::registry()
        .with(BufferLayer {
            buffer: buffer.clone(),
        })
        .with(file_layer.ok());
    if console {
        let subscriber = registry.with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_level(true),
        );
        let _ = tracing::subscriber::set_global_default(subscriber);
    } else {
        let _ = tracing::subscriber::set_global_default(registry);
    }
}

/// 日志文件地址（`%APPDATA%\stereo-link\logs\stereo-link.log`）。
pub fn log_path() -> std::path::PathBuf {
    crate::control::log_path()
}

/// 轮转阈值：单个文件最多留 2 MB，再多就滚到 `.1`（够看最近几十分钟）。
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// 文件层：每条事件追加一行 `LEVEL text`。
struct FileLayer {
    file: Mutex<std::fs::File>,
}

impl FileLayer {
    fn open() -> std::io::Result<Self> {
        let path = log_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0) > MAX_LOG_BYTES {
            let rotated = path.with_extension("log.1");
            let _ = std::fs::rename(&path, &rotated);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }
}

impl<S: Subscriber> Layer<S> for FileLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        if visitor.message.is_empty() {
            return;
        }
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "[{}] {}", event.metadata().level(), visitor.message);
        }
    }
}

struct BufferLayer {
    buffer: LogBuffer,
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for BufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        if visitor.message.is_empty() {
            return;
        }
        self.buffer.push(*event.metadata().level(), visitor.message);
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl MessageVisitor {
    fn field(&mut self, name: &str, value: &str) {
        if name == "message" {
            self.message.push_str(value);
        } else {
            if !self.message.is_empty() {
                self.message.push(' ');
            }
            self.message.push_str(name);
            self.message.push('=');
            self.message.push_str(value);
        }
    }
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.field(field.name(), value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // tracing 的 `message` 字段是 Arguments，Debug 打印出来就是拼好的明文，不带引号。
        self.field(field.name(), &format!("{value:?}"));
    }
}
