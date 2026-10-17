//! 内存日志缓冲：`tracing` 的事件同时进 GUI，不需要用户去看控制台。
//!
//! GUI 进程是 `windows_subsystem = "windows"`（不弹黑框），所以日志面板就是唯一的
//! 诊断出口：采集端点、投放结果、下发失败原因、客户端连没连上都在这里。

use std::collections::VecDeque;
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

    /// 清空。
    pub fn clear(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.lines.clear();
        }
    }
}

/// 把日志缓冲接到全局 `tracing` 订阅器上。
///
/// 默认只进内存（GUI 正式版没有控制台）。调试时设 `MIPLAY_LOG_CONSOLE=1`
/// 会额外往控制台打一份，方便命令行/脚本抓日志。
pub fn install(buffer: LogBuffer) {
    use tracing_subscriber::layer::SubscriberExt;

    let layer = BufferLayer { buffer };
    let registry = tracing_subscriber::registry().with(layer);
    if std::env::var_os("MIPLAY_LOG_CONSOLE").is_some() {
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
        self.buffer
            .push(*event.metadata().level(), visitor.message);
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
