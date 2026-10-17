//! 环形缓冲：单写者 / 多读者，读者各自持有游标。
//!
//! 语义与 Python 参考实现 `pc_stereo_hub.py` 的 `RingBuffer` 对齐：
//! * 容量按秒配置（默认 5 秒），满了丢最旧的数据；
//! * `read_from(pos, ..)` 里 `pos` 是**绝对**写位置（字节）；
//! * 新读者传 `cursor()`（当前写位置）= 从现在开始听，不会补历史；
//! * 读者落后超过容量时**跳帧**到最旧可用位置，并在返回值里报告新位置；
//! * 缓冲被 `close()` 后，读者读完剩余数据会立刻拿到空结果（不阻塞）。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// 一次读取的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOutcome {
    /// 读到的 PCM 字节（可能为空，表示超时或被关闭）。
    pub data: Vec<u8>,
    /// 这次读取之后读者应该使用的下一个位置。
    pub next: u64,
    /// 因为落后太多被丢弃的字节数（跳帧）。
    pub skipped: u64,
    /// 缓冲是否已关闭（此时 `data` 是关闭前剩下的最后一段）。
    pub closed: bool,
}

impl ReadOutcome {
    /// 跳帧的时长（秒），按给定字节率换算。
    pub fn skipped_seconds(&self, bytes_per_second: u32) -> f64 {
        if bytes_per_second == 0 {
            0.0
        } else {
            self.skipped as f64 / bytes_per_second as f64
        }
    }
}

#[derive(Debug)]
struct State {
    buf: Vec<u8>,
    /// 绝对写位置（自 RingBuffer 创建以来写入的总字节数）。
    total: u64,
    closed: bool,
}

/// 固定容量的环形缓冲。
#[derive(Debug)]
pub struct RingBuffer {
    state: Mutex<State>,
    cv: Condvar,
    capacity: usize,
    /// 单调递增的「写入代次」，只用于测试/诊断。
    writes: AtomicU64,
    closed_flag: AtomicBool,
}

impl RingBuffer {
    /// 建一个能装 `capacity_bytes` 的缓冲。
    pub fn new(capacity_bytes: usize) -> Self {
        let cap = capacity_bytes.max(1);
        Self {
            state: Mutex::new(State {
                buf: Vec::with_capacity(cap),
                total: 0,
                closed: false,
            }),
            cv: Condvar::new(),
            capacity: cap,
            writes: AtomicU64::new(0),
            closed_flag: AtomicBool::new(false),
        }
    }

    /// 按「秒 × 字节率」建缓冲。
    pub fn for_seconds(seconds: f32, bytes_per_second: u32) -> Self {
        let cap = (bytes_per_second as f64 * seconds.max(0.1) as f64) as usize;
        Self::new(cap)
    }

    /// 容量（字节）。
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 当前可读的字节数（最旧可用位置到写位置之间的数据）。
    pub fn available(&self) -> usize {
        let st = self.state.lock().unwrap();
        st.buf.len()
    }

    /// 当前写位置（绝对字节偏移）。
    pub fn cursor(&self) -> u64 {
        self.state.lock().unwrap().total
    }

    /// 最旧可用位置的绝对偏移。
    pub fn earliest_cursor(&self) -> u64 {
        let st = self.state.lock().unwrap();
        st.total - st.buf.len() as u64
    }

    /// 累计写入的字节数。
    pub fn writes(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }

    /// 是否已经被 [`RingBuffer::close`] 关掉。
    pub fn is_closed(&self) -> bool {
        self.closed_flag.load(Ordering::Acquire)
    }

    /// 写入数据；超出容量的部分保留最新的 `capacity` 字节（丢最旧）。
    pub fn write(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut st = self.state.lock().unwrap();
        if st.closed {
            return;
        }
        if data.len() >= self.capacity {
            st.buf.clear();
            st.buf
                .extend_from_slice(&data[data.len() - self.capacity..]);
        } else {
            let overflow = (st.buf.len() + data.len()).saturating_sub(self.capacity);
            if overflow > 0 {
                st.buf.drain(..overflow);
            }
            st.buf.extend_from_slice(data);
        }
        st.total += data.len() as u64;
        drop(st);
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.cv.notify_all();
    }

    /// 从绝对位置 `pos` 读最多 `max_bytes`，最多等 `timeout`。
    ///
    /// * 如果 `pos` 比最旧可用数据还旧，跳帧到最旧可用位置并在 [`ReadOutcome::skipped`] 里报告；
    /// * 如果 `pos` 比写位置还新，等到有新数据或者超时/关闭；
    /// * 缓冲关闭后不再阻塞，返回剩余数据（可能为空）。
    pub fn read_from(
        &self,
        pos: u64,
        max_bytes: usize,
        timeout: Duration,
    ) -> Result<ReadOutcome, crate::AudioError> {
        if max_bytes == 0 {
            return Err(crate::AudioError::invalid("max_bytes 必须大于 0"));
        }
        let deadline = Instant::now() + timeout;
        let mut skipped = 0u64;
        let mut st = self.state.lock().unwrap();

        loop {
            let total = st.total;
            let start = total - st.buf.len() as u64;
            let mut eff = pos + skipped;
            if eff < start {
                skipped += start - eff;
                eff = start;
            }
            if eff < total {
                let offset = (eff - start) as usize;
                let avail = st.buf.len() - offset;
                let take = avail.min(max_bytes);
                let data = st.buf[offset..offset + take].to_vec();
                return Ok(ReadOutcome {
                    data,
                    next: eff + take as u64,
                    skipped,
                    closed: st.closed,
                });
            }
            if st.closed {
                return Ok(ReadOutcome {
                    data: Vec::new(),
                    next: eff,
                    skipped,
                    closed: true,
                });
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(ReadOutcome {
                    data: Vec::new(),
                    next: eff,
                    skipped,
                    closed: false,
                });
            }
            let (guard, _) = self.cv.wait_timeout(st, deadline - now).unwrap();
            st = guard;
        }
    }

    /// 关闭缓冲：唤醒所有读者，之后的 [`RingBuffer::write`] 被忽略。
    pub fn close(&self) {
        let mut st = self.state.lock().unwrap();
        st.closed = true;
        drop(st);
        self.closed_flag.store(true, Ordering::Release);
        self.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(50);

    #[test]
    fn new_reader_starts_at_current_position() {
        let rb = RingBuffer::new(1024);
        rb.write(b"old-data");
        let pos = rb.cursor();
        assert_eq!(pos, 8);
        // 新读者从 cursor() 开始：此刻没有数据
        let out = rb.read_from(pos, 1024, MS).unwrap();
        assert!(out.data.is_empty());
        assert_eq!(out.next, 8);
        rb.write(b"new");
        let out = rb.read_from(out.next, 1024, MS).unwrap();
        assert_eq!(out.data, b"new");
        assert_eq!(out.next, 11);
        assert_eq!(out.skipped, 0);
    }

    #[test]
    fn wrap_around_keeps_newest_data() {
        let rb = RingBuffer::new(10);
        rb.write(b"0123456789");
        rb.write(b"ABC");
        assert_eq!(rb.available(), 10);
        // 最旧的 "012" 被丢掉，最旧可用位置 = 3
        let out = rb.read_from(0, 100, MS).unwrap();
        assert_eq!(out.data, b"3456789ABC");
        assert_eq!(out.next, 13);
        assert_eq!(out.skipped, 3);
        assert_eq!(out.skipped_seconds(13), 3.0 / 13.0);
    }

    #[test]
    fn single_write_larger_than_capacity_keeps_tail() {
        let rb = RingBuffer::new(4);
        rb.write(b"abcdefgh");
        let out = rb.read_from(0, 100, MS).unwrap();
        assert_eq!(out.data, b"efgh");
        assert_eq!(out.next, 8);
        assert_eq!(out.skipped, 4);
        assert_eq!(rb.earliest_cursor(), 4);
    }

    #[test]
    fn slow_reader_skips_frames_without_stalling_writer() {
        let rb = RingBuffer::new(8);
        let mut pos = 0u64;
        for i in 0..5u8 {
            rb.write(&[b'a' + i; 4]);
            // 读者故意不动
        }
        let out = rb.read_from(pos, 8, MS).unwrap();
        assert_eq!(out.skipped, 12); // 20 字节写入 - 8 字节容量
        assert_eq!(out.data, b"ddddeeee");
        pos = out.next;
        assert_eq!(pos, 20);
        rb.write(b"ffff");
        let out = rb.read_from(pos, 8, MS).unwrap();
        assert_eq!(out.data, b"ffff");
        assert_eq!(out.skipped, 0);
    }

    #[test]
    fn read_waits_then_returns_when_data_arrives() {
        let rb = std::sync::Arc::new(RingBuffer::new(64));
        let w = rb.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            w.write(b"hello");
        });
        let t0 = Instant::now();
        let out = rb.read_from(0, 64, Duration::from_secs(2)).unwrap();
        assert_eq!(out.data, b"hello");
        assert!(t0.elapsed() >= Duration::from_millis(60));
        t.join().unwrap();
    }

    #[test]
    fn read_times_out_on_empty_buffer() {
        let rb = RingBuffer::new(64);
        let t0 = Instant::now();
        let out = rb.read_from(0, 64, Duration::from_millis(60)).unwrap();
        assert!(out.data.is_empty());
        assert!(!out.closed);
        let el = t0.elapsed();
        assert!(el >= Duration::from_millis(50), "elapsed={el:?}");
        assert!(el < Duration::from_secs(1), "elapsed={el:?}");
    }

    #[test]
    fn close_wakes_blocked_reader_and_ignores_later_writes() {
        let rb = std::sync::Arc::new(RingBuffer::new(64));
        rb.write(b"last");
        let r = rb.clone();
        let t = std::thread::spawn(move || r.read_from(4, 64, Duration::from_secs(5)).unwrap());
        std::thread::sleep(Duration::from_millis(50));
        rb.close();
        let out = t.join().unwrap();
        assert!(out.closed);
        assert!(out.data.is_empty());
        assert!(rb.is_closed());
        rb.write(b"ignored");
        assert_eq!(rb.cursor(), 4);
    }

    #[test]
    fn partial_read_advances_by_bytes_read() {
        let rb = RingBuffer::new(64);
        rb.write(b"0123456789");
        let a = rb.read_from(0, 4, MS).unwrap();
        assert_eq!(a.data, b"0123");
        assert_eq!(a.next, 4);
        let b = rb.read_from(a.next, 100, MS).unwrap();
        assert_eq!(b.data, b"456789");
        assert_eq!(b.next, 10);
    }

    #[test]
    fn zero_max_bytes_is_rejected() {
        let rb = RingBuffer::new(8);
        assert!(rb.read_from(0, 0, MS).is_err());
    }

    #[test]
    fn capacity_and_for_seconds() {
        let rb = RingBuffer::for_seconds(5.0, 44_100 * 4);
        assert_eq!(rb.capacity(), 882_000);
        assert_eq!(RingBuffer::new(0).capacity(), 1);
    }
}
