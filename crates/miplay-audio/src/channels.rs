//! 声道分发：把 2ch s16le 输入按配置映射成 N 条独立流（每条流一个 [`RingBuffer`]）。
//!
//! 对应 Python 参考实现里的「左右分离」：`left→音箱A`、`right→音箱B`
//! （每条流都复制成 2ch，保证音箱/浏览器两侧都能正常解码）。

use std::sync::{Arc, Mutex};

use crate::convert::f32_to_s16;
use crate::error::{AudioError, Result};
use crate::ring::RingBuffer;

/// 一条输出流从输入立体声里取什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelSource {
    /// 只取左声道，复制成 2ch。
    Left,
    /// 只取右声道，复制成 2ch。
    Right,
    /// 左右平均后复制成 2ch。
    Mixdown,
    /// 原样立体声（L/R 保留）。
    Stereo,
}

impl ChannelSource {
    /// `"left"` / `"right"` / `"mixdown"` / `"stereo"`。
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelSource::Left => "left",
            ChannelSource::Right => "right",
            ChannelSource::Mixdown => "mixdown",
            ChannelSource::Stereo => "stereo",
        }
    }
}

/// 一条分发规则。
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelRoute {
    /// 目标 id（HTTP 层用它做 URL 路径，例如 `left` / `right` / 音箱名）。
    pub id: String,
    /// 这条流从输入立体声里取什么。
    pub source: ChannelSource,
    /// 线性增益，默认 1.0。
    pub gain: f32,
}

impl ChannelRoute {
    /// 新建一条路由（默认增益 1.0）。
    pub fn new(id: impl Into<String>, source: ChannelSource) -> Self {
        Self {
            id: id.into(),
            source,
            gain: 1.0,
        }
    }

    /// 设置线性增益（1.0 = 原始电平）。
    pub fn with_gain(mut self, gain: f32) -> Self {
        self.gain = gain;
        self
    }
}

/// 「左→A，右→B」这种最常见的双目标配置。
pub fn stereo_routes(
    left_id: impl Into<String>,
    right_id: impl Into<String>,
) -> Vec<ChannelRoute> {
    vec![
        ChannelRoute::new(left_id, ChannelSource::Left),
        ChannelRoute::new(right_id, ChannelSource::Right),
    ]
}

/// 每个 id 一条混音流（所有目标听同样的内容）。
pub fn mixdown_routes<I, S>(ids: I) -> Vec<ChannelRoute>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    ids.into_iter()
        .map(|id| ChannelRoute::new(id, ChannelSource::Mixdown))
        .collect()
}

#[derive(Debug)]
struct RouteState {
    route: ChannelRoute,
    ring: Arc<RingBuffer>,
}

/// 2ch s16le 输入 → N 条环形缓冲流。
#[derive(Debug)]
pub struct ChannelRouter {
    routes: Vec<RouteState>,
    /// 复用的输出暂存区，避免每块都分配。
    scratch: Mutex<Vec<u8>>,
    frames: std::sync::atomic::AtomicU64,
}

impl ChannelRouter {
    /// 用显式容量（字节）建路由器。
    pub fn new(routes: impl IntoIterator<Item = ChannelRoute>, capacity_bytes: usize) -> Self {
        let routes = routes
            .into_iter()
            .map(|route| RouteState {
                route,
                ring: Arc::new(RingBuffer::new(capacity_bytes)),
            })
            .collect();
        Self {
            routes,
            scratch: Mutex::new(Vec::new()),
            frames: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 按「每条流 N 秒」建路由器（默认 5 秒，与 Python 版一致）。
    pub fn for_seconds(
        routes: impl IntoIterator<Item = ChannelRoute>,
        samplerate: u32,
        channels: u16,
        seconds: f32,
    ) -> Self {
        let bytes_per_second = samplerate * channels as u32 * 2;
        Self::new(
            routes,
            (bytes_per_second as f64 * seconds.max(0.1) as f64) as usize,
        )
    }

    /// 写一块 2ch s16le PCM，返回处理掉的帧数（尾部不足一帧的字节被忽略）。
    pub fn write_s16_stereo(&self, pcm: &[u8]) -> usize {
        let frames = pcm.len() / 4;
        if frames == 0 {
            return 0;
        }
        {
            let mut scratch = self.scratch.lock().unwrap();
            for r in &self.routes {
                scratch.clear();
                scratch.reserve(frames * 4);
                for f in 0..frames {
                    let off = f * 4;
                    let l = i16::from_le_bytes([pcm[off], pcm[off + 1]]);
                    let rr = i16::from_le_bytes([pcm[off + 2], pcm[off + 3]]);
                    let (a, b) = match r.route.source {
                        ChannelSource::Left => (l, l),
                        ChannelSource::Right => (rr, rr),
                        ChannelSource::Stereo => (l, rr),
                        ChannelSource::Mixdown => {
                            let m = ((l as i32 + rr as i32) / 2) as i16;
                            (m, m)
                        }
                    };
                    let (a, b) = (apply_gain(a, r.route.gain), apply_gain(b, r.route.gain));
                    scratch.extend_from_slice(&a.to_le_bytes());
                    scratch.extend_from_slice(&b.to_le_bytes());
                }
                r.ring.write(&scratch);
            }
        }
        self.frames
            .fetch_add(frames as u64, std::sync::atomic::Ordering::Relaxed);
        frames
    }

    /// 累计处理过的帧数。
    pub fn frames(&self) -> u64 {
        self.frames.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 规则列表。
    pub fn routes(&self) -> Vec<ChannelRoute> {
        self.routes.iter().map(|r| r.route.clone()).collect()
    }

    /// 所有输出 id，顺序与规则一致。
    pub fn ids(&self) -> Vec<String> {
        self.routes.iter().map(|r| r.route.id.clone()).collect()
    }

    /// 取某条流的环形缓冲（HTTP 层每个连接读它）。
    pub fn ring(&self, id: &str) -> Option<Arc<RingBuffer>> {
        self.routes
            .iter()
            .find(|r| r.route.id == id)
            .map(|r| r.ring.clone())
    }

    /// 所有流：(id, ring)。
    pub fn rings(&self) -> Vec<(String, Arc<RingBuffer>)> {
        self.routes
            .iter()
            .map(|r| (r.route.id.clone(), r.ring.clone()))
            .collect()
    }

    /// 关闭所有流（唤醒阻塞的读者）。
    pub fn close(&self) {
        for r in &self.routes {
            r.ring.close();
        }
    }

    /// 校验规则（id 唯一、增益有限、至少有 1 条、声道源与输入匹配）。
    pub fn validate(routes: &[ChannelRoute]) -> Result<()> {
        if routes.is_empty() {
            return Err(AudioError::invalid("至少要有一条分发规则"));
        }
        let mut seen = std::collections::HashSet::new();
        for r in routes {
            if r.id.trim().is_empty() {
                return Err(AudioError::invalid("分发规则的 id 不能为空"));
            }
            if !r.gain.is_finite() {
                return Err(AudioError::invalid(format!("id={} 的增益不是有限数", r.id)));
            }
            if !seen.insert(r.id.clone()) {
                return Err(AudioError::invalid(format!("分发规则 id 重复：{}", r.id)));
            }
        }
        Ok(())
    }
}

#[inline]
fn apply_gain(sample: i16, gain: f32) -> i16 {
    if (gain - 1.0).abs() < f32::EPSILON {
        sample
    } else {
        f32_to_s16(crate::convert::s16_to_f32(sample) * gain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(l: i16, r: i16) -> [u8; 4] {
        let mut b = [0u8; 4];
        b[0..2].copy_from_slice(&l.to_le_bytes());
        b[2..4].copy_from_slice(&r.to_le_bytes());
        b
    }

    fn stereo_pcm(pairs: &[(i16, i16)]) -> Vec<u8> {
        let mut v = Vec::new();
        for (l, r) in pairs {
            v.extend_from_slice(&frame(*l, *r));
        }
        v
    }

    fn read_all(rb: &RingBuffer) -> Vec<u8> {
        rb.read_from(0, usize::MAX, std::time::Duration::from_millis(1))
            .unwrap()
            .data
    }

    #[test]
    fn left_and_right_routes_are_split_and_duplicated() {
        let router = ChannelRouter::new(stereo_routes("a", "b"), 4096);
        let pcm = stereo_pcm(&[(100, -100), (200, -200)]);
        assert_eq!(router.write_s16_stereo(&pcm), 2);
        let a = read_all(&router.ring("a").unwrap());
        let b = read_all(&router.ring("b").unwrap());
        assert_eq!(a, stereo_pcm(&[(100, 100), (200, 200)]));
        assert_eq!(b, stereo_pcm(&[(-100, -100), (-200, -200)]));
        assert_eq!(router.ids(), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(router.frames(), 2);
    }

    #[test]
    fn mixdown_and_stereo_routes() {
        let routes = vec![
            ChannelRoute::new("m", ChannelSource::Mixdown),
            ChannelRoute::new("s", ChannelSource::Stereo),
        ];
        ChannelRouter::validate(&routes).unwrap();
        let router = ChannelRouter::new(routes, 4096);
        router.write_s16_stereo(&stereo_pcm(&[(100, 300), (-100, 101)]));
        assert_eq!(
            read_all(&router.ring("m").unwrap()),
            stereo_pcm(&[(200, 200), (0, 0)])
        );
        assert_eq!(
            read_all(&router.ring("s").unwrap()),
            stereo_pcm(&[(100, 300), (-100, 101)])
        );
    }

    #[test]
    fn gain_scales_output() {
        let routes = vec![ChannelRoute::new("half", ChannelSource::Left).with_gain(0.5)];
        let router = ChannelRouter::new(routes, 4096);
        router.write_s16_stereo(&stereo_pcm(&[(20000, 0)]));
        assert_eq!(read_all(&router.ring("half").unwrap()), stereo_pcm(&[(10000, 10000)]));
    }

    #[test]
    fn ring_capacity_is_per_route_and_shared_by_arc() {
        let router = ChannelRouter::for_seconds(stereo_routes("a", "b"), 44_100, 2, 5.0);
        let a = router.ring("a").unwrap();
        assert_eq!(a.capacity(), 44_100 * 4 * 5);
        assert!(Arc::ptr_eq(&a, &router.ring("a").unwrap()));
        assert!(router.ring("nope").is_none());
        assert_eq!(router.rings().len(), 2);
    }

    #[test]
    fn trailing_partial_frame_is_ignored() {
        let router = ChannelRouter::new(stereo_routes("a", "b"), 4096);
        let mut pcm = stereo_pcm(&[(1, 2)]);
        pcm.extend_from_slice(&[9, 9, 9]);
        assert_eq!(router.write_s16_stereo(&pcm), 1);
        assert_eq!(router.write_s16_stereo(&[1, 2, 3]), 0);
    }

    #[test]
    fn close_closes_every_route() {
        let router = ChannelRouter::new(stereo_routes("a", "b"), 4096);
        router.write_s16_stereo(&stereo_pcm(&[(1, 2)]));
        router.close();
        assert!(router.ring("a").unwrap().is_closed());
        assert!(router.ring("b").unwrap().is_closed());
        let out = router
            .ring("a")
            .unwrap()
            .read_from(0, 100, std::time::Duration::from_millis(1))
            .unwrap();
        assert_eq!(out.data, stereo_pcm(&[(1, 1)]));
        assert!(out.closed);
    }

    #[test]
    fn validate_rejects_bad_configs() {
        assert!(ChannelRouter::validate(&[]).is_err());
        assert!(ChannelRouter::validate(&[
            ChannelRoute::new("a", ChannelSource::Left),
            ChannelRoute::new("a", ChannelSource::Right),
        ])
        .is_err());
        assert!(ChannelRouter::validate(&[ChannelRoute::new("  ", ChannelSource::Left)]).is_err());
        assert!(ChannelRouter::validate(&[ChannelRoute::new("a", ChannelSource::Left).with_gain(f32::NAN)]).is_err());
        assert!(ChannelRouter::validate(&stereo_routes("l", "r")).is_ok());
    }

    #[test]
    fn mixdown_routes_helper_builds_one_per_id() {
        let routes = mixdown_routes(["客厅", "书房"]);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].source, ChannelSource::Mixdown);
        assert_eq!(routes[1].id, "书房");
    }

    #[test]
    fn new_reader_starts_now_and_skips_stale_audio() {
        let router = ChannelRouter::new(stereo_routes("a", "b"), 4 * 4); // 4 帧容量
        for _ in 0..10 {
            router.write_s16_stereo(&stereo_pcm(&[(1000, 2000)]));
        }
        let ring = router.ring("a").unwrap();
        let pos = ring.cursor();
        assert_eq!(pos, 40);
        let out = ring
            .read_from(pos, 1024, std::time::Duration::from_millis(5))
            .unwrap();
        assert!(out.data.is_empty());
        router.write_s16_stereo(&stereo_pcm(&[(3000, 4000)]));
        let out = ring
            .read_from(pos, 1024, std::time::Duration::from_millis(50))
            .unwrap();
        assert_eq!(out.data, stereo_pcm(&[(3000, 3000)]));
    }
}
