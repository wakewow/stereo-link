//! 延迟模型：把「本机 → 出声」拆成可解释的几项，并给出哪些项是实测/上报的、哪些是未知的。
//!
//! 关键事实（来自 2026-10-17 对 `192.168.1.21` 的真机探测，见 `research/lan/lan-paths.md`）：
//! 设备在 `SETUP` 里给出实时流的接受窗口 **`latencyMin = 11025` 帧（250 ms）**、
//! **`latencyMax = 88200` 帧（2000 ms）**（44.1 kHz）。AirPlay 的实时流是**由发送端声明延迟**的：
//! 发送端在每个同步包（sync packet）里写 `now_without_latency = 下一个包的 RTP 时间戳 - latency_frames`，
//! 接收端据此把音频压后 `latency_frames` 再渲染。所以「我们声明多少」直接决定端到端延迟的下限，
//! 而**不需要**等接收端协商一个更大的缓冲。

/// 实时流采样率（Hz）。
pub const SAMPLE_RATE: u32 = 44_100;

/// 设备接受的最小声明延迟：11025 帧 = 250 ms。
pub const DEVICE_LATENCY_MIN_FRAMES: u32 = 11_025;
/// 设备接受的最大声明延迟：88200 帧 = 2000 ms。
pub const DEVICE_LATENCY_MAX_FRAMES: u32 = 88_200;
/// [`DEVICE_LATENCY_MIN_FRAMES`] 对应的毫秒数。
pub const DEVICE_LATENCY_MIN_MS: u32 = 250;
/// [`DEVICE_LATENCY_MAX_FRAMES`] 对应的毫秒数。
pub const DEVICE_LATENCY_MAX_MS: u32 = 2000;

/// 实时流每个 RTP 包的帧数（44.1 kHz 下 352 帧 ≈ 7.98 ms）。
pub const PACKET_FRAMES: u32 = 352;

/// 本 crate 的目标端到端延迟（毫秒）。
pub const TARGET_TOTAL_MS: f64 = 400.0;

/// 帧数 → 毫秒。
#[must_use]
pub fn frames_to_ms(frames: u32, sample_rate: u32) -> f64 {
    f64::from(frames) * 1000.0 / f64::from(sample_rate)
}

/// 毫秒 → 帧数（四舍五入）。
#[must_use]
pub fn ms_to_frames(ms: u32, sample_rate: u32) -> u32 {
    (u64::from(ms) * u64::from(sample_rate) / 1000) as u32
}

/// 一条通路的延迟构成。
///
/// 加性项只有三项（顺序＝信号经过的顺序）：
/// `queue_ms`（我们自己缓冲的） + `declared_ms`（接收端按声明压后的） + `device_render_ms`（设备内部渲染）。
///
/// 网络单程与发送提前量（send-ahead）**不加**：只要它们小于 `declared_ms`，就被声明窗口吸收；
/// 这也是为什么声明延迟必须显著大于局域网抖动。
#[derive(Debug, Clone, PartialEq)]
pub struct LatencyBudget {
    /// 我们在同步包里声明的延迟（帧）。
    pub declared_frames: u32,
    /// 同上，换算成毫秒。
    pub declared_ms: f64,
    /// 一个 RTP 包的时长（毫秒）；稳态下排队深度约为 1 个包。
    pub packet_ms: f64,
    /// 我们这一侧实际排队深度（毫秒），由调用方按运行期实测值填。
    pub queue_ms: f64,
    /// 设备在 `SETUP` 响应里上报的内部渲染延迟（毫秒）；未上报则 `None`。
    pub device_render_ms: Option<f64>,
    /// 发送提前量（毫秒）：仅用于抹平 Windows 定时器抖动，被声明窗口吸收，不计入总延迟。
    pub send_ahead_ms: f64,
}

impl LatencyBudget {
    /// 组装一份延迟预算。
    #[must_use]
    pub fn new(
        declared_frames: u32,
        packet_frames: u32,
        queue_ms: f64,
        send_ahead_packets: u64,
        device_render_ms: Option<f64>,
    ) -> Self {
        let packet_ms = frames_to_ms(packet_frames, SAMPLE_RATE);
        Self {
            declared_frames,
            declared_ms: frames_to_ms(declared_frames, SAMPLE_RATE),
            packet_ms,
            queue_ms,
            device_render_ms,
            send_ahead_ms: packet_ms * send_ahead_packets as f64,
        }
    }

    /// 未上报设备渲染延迟时的保守估计（毫秒）。
    ///
    /// 取 100 ms 作为占位：AirPlay 接收端的 `arrivalToRenderLatency` 通常在这个量级。
    /// **这是估计值，不是实测值**，[`Self::unknown_terms`] 会把它标出来。
    pub const ASSUMED_DEVICE_RENDER_MS: f64 = 100.0;

    /// 端到端估计值（毫秒）。`assume_device_render` 为真时用
    /// [`Self::ASSUMED_DEVICE_RENDER_MS`] 顶替未上报的设备渲染延迟。
    #[must_use]
    pub fn estimated_total_ms(&self, assume_device_render: bool) -> f64 {
        let device = self.device_render_ms.unwrap_or(if assume_device_render {
            Self::ASSUMED_DEVICE_RENDER_MS
        } else {
            0.0
        });
        self.queue_ms + self.declared_ms + device
    }

    /// 还差多少毫秒到 [`TARGET_TOTAL_MS`]（为负表示已达标）。
    #[must_use]
    pub fn gap_to_target_ms(&self, assume_device_render: bool) -> f64 {
        self.estimated_total_ms(assume_device_render) - TARGET_TOTAL_MS
    }

    /// 未知/未实测的项（每条都说明为什么未知）。
    #[must_use]
    pub fn unknown_terms(&self) -> Vec<&'static str> {
        let mut terms = vec![
            "Windows WASAPI 环回采集缓冲（由 miplay-audio 决定，本 crate 之外）",
            "音箱 DAC/功放输出级（无仪器可测）",
        ];
        if self.device_render_ms.is_none() {
            terms.push("音箱内部渲染延迟（本次 SETUP 未上报 arrival_to_render_latency_ms）");
        }
        terms
    }

    /// 一行摘要，便于日志与报告引用。
    #[must_use]
    pub fn summary(&self, assume_device_render: bool) -> String {
        let device = match self.device_render_ms {
            Some(ms) => format!("{ms:.0} ms（设备上报）"),
            None => format!("{}（未上报）", if assume_device_render {
                format!("{:.0} ms（估计）", Self::ASSUMED_DEVICE_RENDER_MS)
            } else {
                "未计入".to_string()
            }),
        };
        format!(
            "排队 {:.1} + 声明 {:.1} + 设备渲染 {} = 约 {:.0} ms（目标 ≤{:.0} ms，差 {:+.0} ms；\
             发送提前 {:.1} ms 与网络单程被声明窗口吸收）",
            self.queue_ms,
            self.declared_ms,
            device,
            self.estimated_total_ms(assume_device_render),
            TARGET_TOTAL_MS,
            self.gap_to_target_ms(assume_device_render),
            self.send_ahead_ms,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_window_is_quarter_second_to_two_seconds() {
        assert_eq!(frames_to_ms(DEVICE_LATENCY_MIN_FRAMES, SAMPLE_RATE), 250.0);
        assert_eq!(frames_to_ms(DEVICE_LATENCY_MAX_FRAMES, SAMPLE_RATE), 2000.0);
        assert_eq!(ms_to_frames(DEVICE_LATENCY_MIN_MS, SAMPLE_RATE), 11_025);
        assert_eq!(ms_to_frames(DEVICE_LATENCY_MAX_MS, SAMPLE_RATE), 88_200);
        assert_eq!(DEVICE_LATENCY_MIN_MS, 250);
        assert_eq!(DEVICE_LATENCY_MAX_MS, 2000);
    }

    #[test]
    fn ms_and_frames_round_trip() {
        for ms in [1u32, 7, 8, 20, 100, 250, 400, 1000, 2000] {
            let frames = ms_to_frames(ms, SAMPLE_RATE);
            let back = frames_to_ms(frames, SAMPLE_RATE);
            assert!((back - f64::from(ms)).abs() < 0.02, "{ms} ms -> {back} ms");
        }
    }

    #[test]
    fn packet_is_about_eight_milliseconds() {
        let budget = LatencyBudget::new(DEVICE_LATENCY_MIN_FRAMES, PACKET_FRAMES, 0.0, 2, Some(0.0));
        assert!((budget.packet_ms - 7.98).abs() < 0.01, "{}", budget.packet_ms);
        assert!((budget.send_ahead_ms - 15.96).abs() < 0.02);
    }

    #[test]
    fn total_is_the_sum_of_the_three_additive_terms() {
        let budget = LatencyBudget::new(DEVICE_LATENCY_MIN_FRAMES, PACKET_FRAMES, 8.0, 2, Some(0.0));
        assert!((budget.estimated_total_ms(true) - 258.0).abs() < 0.001);
        assert!(!budget.unknown_terms().iter().any(|t| t.contains("渲染")));
    }

    #[test]
    fn missing_device_render_shows_up_as_unknown_and_as_estimate() {
        let budget = LatencyBudget::new(DEVICE_LATENCY_MIN_FRAMES, PACKET_FRAMES, 8.0, 2, None);
        assert!((budget.estimated_total_ms(false) - 258.0).abs() < 0.001);
        assert!((budget.estimated_total_ms(true) - 358.0).abs() < 0.001);
        assert!(budget
            .unknown_terms()
            .iter()
            .any(|t| t.contains("arrival_to_render_latency_ms")));
    }

    #[test]
    fn floor_latency_meets_the_four_hundred_millisecond_target() {
        let budget = LatencyBudget::new(DEVICE_LATENCY_MIN_FRAMES, PACKET_FRAMES, 8.0, 2, None);
        assert!(budget.gap_to_target_ms(true) < 0.0, "{}", budget.summary(true));
        assert!(
            budget.estimated_total_ms(true) <= TARGET_TOTAL_MS,
            "取窗口下限就该进 400 ms"
        );
    }

    #[test]
    fn doubling_declared_latency_costs_exactly_the_difference() {
        let low = LatencyBudget::new(DEVICE_LATENCY_MIN_FRAMES, PACKET_FRAMES, 8.0, 2, Some(50.0));
        let high = LatencyBudget::new(DEVICE_LATENCY_MAX_FRAMES, PACKET_FRAMES, 8.0, 2, Some(50.0));
        assert!(
            (high.estimated_total_ms(false) - low.estimated_total_ms(false) - 1750.0).abs() < 0.001
        );
        assert!(high.gap_to_target_ms(false) > 0.0, "2 秒声明必然超标");
    }

    #[test]
    fn summary_mentions_every_term() {
        let budget = LatencyBudget::new(DEVICE_LATENCY_MIN_FRAMES, PACKET_FRAMES, 8.0, 2, Some(30.0));
        let text = budget.summary(false);
        assert!(text.contains("声明"), "{text}");
        assert!(text.contains("设备渲染"), "{text}");
        assert!(text.contains("设备上报"), "{text}");
    }
}
