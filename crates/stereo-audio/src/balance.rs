//! 音量平衡校准：把「两台音箱的响度差」算成**引擎侧数字增益**，而不是去改端点音量。
//!
//! ## 为什么改端点音量这条路走不通
//!
//! 平衡音量以前的做法是：测出左右电平差，把两台音箱的**端点标量**改成一高一低。可是
//! 音量联动（apply_follow）在系统音量变化时会把采集源的标量**原样镜像**到每台目标设备——
//! 两台的标量立刻被写成同一个值，平衡信息就没了。平衡数据只存在于「两个标量之差」里，
//! 而那个差随时会被覆盖，这是结构性的，改阈值、加保护都治不好。
//!
//! 所以平衡改存成「**响度差本身**」（dB），播放时由路由引擎给较响的一路乘一个衰减系数：
//!
//! `text
//! 左端点标量 s_L ─┐
//! 右端点标量 s_R ─┼─→ Δ(s_L, s_R, m) = curve_L(s_L) − curve_R(s_R) + sens(m)
//! 主音量   m     ─┘        ↑ 端点自报曲线差        ↑ 扣掉端点电平后的喇叭灵敏度差
//!
//! 增益：g_L = min(−Δ, 0)，g_R = min(Δ, 0)   —— 只压不抬，重复应用幂等
//! `
//!
//! 跟随生效时两端标量都等于主音量 m，Δ 就退化成 sens(m)：**与主音量无关的固定 dB 偏移**，
//! 所以调系统音量时平衡不会被破坏，反而跟着主音量一起自动刷新（见 stereo-link 的 tick）。
//!
//! ## 为什么需要三样数据
//!
//! 1. **端点曲线**（[EndpointCurve]）：同一标量在不同型号的设备上不是同一个 dB（Windows
//!    的音量曲线不是对数直线）。纯软件扫 SetMasterVolumeLevelScalar + GetMasterVolumeLevel
//!    就能拿到，不需要麦克风、不需要放音。
//! 2. **声学锚点**（[BalanceAnchor]）：在小/中/大三档主音量下各测一次左右电平差。端点曲线
//!    只能解释「端点电平」那部分，喇叭本身的灵敏度差与压缩（音量越大越压）必须靠实测。
//! 3. **灵敏度差** sens(m)：delta_k − (curve_L(m_k) − curve_R(m_k))，再按主音量插值。
//!
//! 校准一次做全：扫曲线 → 多点声学测量 → 存进配置，之后用户调音量**不需要再校准**。

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::error::AudioError;
use crate::sync::{measure_balance, taper_db, BalanceReport, MicCandidate, SyncConfig};
use crate::volume::{
    capture_endpoint_curve, get_endpoint_volume, set_endpoint_mute, set_endpoint_volume,
    EndpointVolume,
};

/// 平衡增益上限（dB）：较响的一路最多压 24 dB，再压就基本听不见了。
pub const BALANCE_GAIN_MAX_DB: f32 = 24.0;

/// 增益死区（dB）：|Δ| <= 0.2 时不补偿，避免界面每帧抖动、反复写引擎原子量。
pub const BALANCE_GAIN_DEADBAND_DB: f32 = 0.2;

/// 默认校准锚点（端点标量）：覆盖小、中、大三档音量。
///
/// 三个点已经能插值出「喇叭压缩」随音量的变化；再多加一个点只会让校准时间线性变长
/// （每个锚点要放两次测试音），收益很小。
pub const BALANCE_ANCHOR_SCALARS: [f32; 3] = [0.30, 0.60, 0.90];

/// 校准数据结构版本（将来公式变了用它做迁移）。
pub const BALANCE_CAL_VERSION: u32 = 1;

/// 一台端点的「标量 → 端点自报 dB」曲线（升序点表，分段线性插值）。
///
/// 由 [capture_endpoint_curve] 扫出来：同一标量在蓝牙 A2DP、Realtek、USB 声卡上可能是不同的
/// dB，而平衡要的正是两台设备之间的**差**，所以必须各扫各的。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EndpointCurve {
    /// 升序的 (标量, dB) 点；空表示没扫到（回退到 [taper_db] 的典型曲线）。
    pub points: Vec<(f32, f32)>,
}

impl EndpointCurve {
    /// 整理点表：丢弃非有限值、标量夹到 0..=1、按标量升序、同一标量只留第一个。
    pub fn new(points: Vec<(f32, f32)>) -> Self {
        let mut points: Vec<(f32, f32)> = points
            .into_iter()
            .filter(|(s, db)| s.is_finite() && db.is_finite())
            .map(|(s, db)| (s.clamp(0.0, 1.0), db))
            .collect();
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        points.dedup_by(|a, b| (a.0 - b.0).abs() < 1e-4);
        Self { points }
    }

    /// 有没有扫到曲线（空 = 只能用典型曲线兜底）。
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// 某个标量下端点自报的 dB；超出点表范围按端点值外推，空曲线回退 [taper_db]。
    pub fn db_at(&self, scalar: f32) -> f32 {
        if !scalar.is_finite() {
            return 0.0;
        }
        let scalar = scalar.clamp(0.0, 1.0);
        if self.points.is_empty() {
            return taper_db(scalar) as f32;
        }
        interp(&self.points, scalar)
    }

    /// 两台端点的电平差（dB，左 − 右）：self 是左端点、other 是右端点。
    pub fn level_diff(&self, other: &EndpointCurve, left_scalar: f32, right_scalar: f32) -> f32 {
        self.db_at(left_scalar) - other.db_at(right_scalar)
    }

    /// 人读一行。
    pub fn describe(&self) -> String {
        if self.points.is_empty() {
            return "未扫描（按典型曲线估算）".to_string();
        }
        let first = self.points[0];
        let last = self.points[self.points.len() - 1];
        format!(
            "{} 点（{:.0}% ≈ {:.1} dB … {:.0}% ≈ {:.1} dB）",
            self.points.len(),
            first.0 * 100.0,
            first.1,
            last.0 * 100.0,
            last.1
        )
    }
}

/// 一次声学锚点测量：在某个主音量下（两端标量相同）左右两路的实测电平差。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BalanceAnchor {
    /// 测量时的端点标量（两端相同，0..=1）。
    pub master: f32,
    /// taper_db(master)，插值用（dB 域比标量域更接近线性）。
    pub master_db: f32,
    /// 实测电平差（dB，左 − 右）：正 = 左边更响。
    pub delta_db: f32,
    /// 实测左路电平（dBFS，麦克风收到的测试音幅度）。
    pub left_db: f32,
    /// 实测右路电平（dBFS）。
    pub right_db: f32,
    /// 这次测量的可信度（0..=1，由测试音信噪比决定）。
    pub confidence: f32,
}

/// 一台音箱的完整校准结果。
///
/// 存进配置后，运行时只需要「当前两端标量 + 主音量」就能算出补偿增益，**不需要再放测试音**。
#[derive(Clone, Debug, PartialEq)]
pub struct BalanceCalibration {
    /// 数据结构版本，见 [BALANCE_CAL_VERSION]。
    pub version: u32,
    /// 校准时左端点的完整友好名（展示用；换设备后这份校准应当作废）。
    pub left_endpoint: String,
    /// 校准时右端点的完整友好名。
    pub right_endpoint: String,
    /// 声学锚点（按 master 升序）。
    pub anchors: Vec<BalanceAnchor>,
    /// 左端点的标量 → dB 曲线。
    pub left_curve: EndpointCurve,
    /// 右端点的标量 → dB 曲线。
    pub right_curve: EndpointCurve,
    /// 扣掉端点电平后的灵敏度差（dB，左 − 右），取满音量（m = 1.0）处的值；锚点缺失时当兜底。
    pub sens_diff_db: f32,
    /// 校准完成时间（Unix 秒），0 = 未知。
    pub calibrated_at: u64,
}

impl Default for BalanceCalibration {
    fn default() -> Self {
        Self {
            version: BALANCE_CAL_VERSION,
            left_endpoint: String::new(),
            right_endpoint: String::new(),
            anchors: Vec::new(),
            left_curve: EndpointCurve::default(),
            right_curve: EndpointCurve::default(),
            sens_diff_db: 0.0,
            calibrated_at: 0,
        }
    }
}

impl BalanceCalibration {
    /// 还没有任何校准数据。
    pub fn is_empty(&self) -> bool {
        self.anchors.is_empty() && self.sens_diff_db == 0.0
    }

    /// 锚点按 master_db 升序排列后的 (master_db, 灵敏度差) 点表。
    fn sens_points(&self) -> Vec<(f32, f32)> {
        let mut points: Vec<(f32, f32)> = self
            .anchors
            .iter()
            .map(|a| {
                let level = self.left_curve.db_at(a.master) - self.right_curve.db_at(a.master);
                (a.master_db, a.delta_db - level)
            })
            .filter(|(m, s)| m.is_finite() && s.is_finite())
            .collect();
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        points
    }

    /// 某个主音量下的灵敏度差（dB，左 − 右）：锚点之间线性插值，两端外推为端点值。
    ///
    /// 锚点缺失时回退到 [Self::sens_diff_db]（老配置/手写配置才有这种状态）。
    pub fn sens_at(&self, master: f32) -> f32 {
        if !master.is_finite() {
            return 0.0;
        }
        let points = self.sens_points();
        if points.is_empty() {
            return if self.sens_diff_db.is_finite() {
                self.sens_diff_db
            } else {
                0.0
            };
        }
        interp(&points, taper_db(master.clamp(0.0, 1.0)) as f32)
    }

    /// 任意「左右标量 + 主音量」组合下的响度差 Δ（dB，左 − 右）。
    ///
    /// 跟随生效时两端标量都等于主音量；用户手动改过某一路路由音量时也能算。
    pub fn delta_at(&self, left_scalar: f32, right_scalar: f32, master: f32) -> f32 {
        let level = self
            .left_curve
            .level_diff(&self.right_curve, left_scalar, right_scalar);
        level + self.sens_at(master)
    }

    /// 对应 [Self::delta_at] 的补偿增益 [左, 右]（dB，均 ≤ 0）。
    pub fn gains_at(&self, left_scalar: f32, right_scalar: f32, master: f32) -> [f32; 2] {
        balance_gains(self.delta_at(left_scalar, right_scalar, master))
    }

    /// 跟随语义下的补偿增益：两端标量都等于主音量。
    pub fn gains_following(&self, master: f32) -> [f32; 2] {
        self.gains_at(master, master, master)
    }

    /// 加上补偿后残差还有多少（dB，左 − 右）：Δ + g_L − g_R。
    ///
    /// 应用补偿后应当落在死区内（≈ 0），用来给界面显示「有效差」。
    pub fn residual_db(&self, left_scalar: f32, right_scalar: f32, master: f32) -> f32 {
        let delta = self.delta_at(left_scalar, right_scalar, master);
        let gains = balance_gains(delta);
        delta + gains[0] - gains[1]
    }

    /// 单个锚点的人读说明（界面展示「不同音量下的差」）。
    pub fn anchor_label(&self, anchor: &BalanceAnchor) -> String {
        format!(
            "{:.0}%：左−右 {:.1} dB（信噪 {:.0}%）",
            anchor.master * 100.0,
            anchor.delta_db,
            anchor.confidence * 100.0
        )
    }

    /// 人读一行（放界面/日志）。
    pub fn describe(&self) -> String {
        if self.is_empty() {
            return "尚未校准".to_string();
        }
        let anchors: Vec<String> = self.anchors.iter().map(|a| self.anchor_label(a)).collect();
        let mut text = format!(
            "{} 个锚点（{}）；灵敏度差 {:+.1} dB（左−右）",
            self.anchors.len(),
            anchors.join("，"),
            self.sens_diff_db
        );
        if !self.left_curve.is_empty() && !self.right_curve.is_empty() {
            text.push_str("；端点曲线已扫描");
        }
        text
    }
}

/// 由响度差 Δ（dB，左 − 右）算补偿增益 [左, 右]。
///
/// 规则（对应「以较安静的一侧为基准，只压不抬」）：
/// * Δ > 0（左边更响）：左压 |Δ|，右不动；
/// * Δ < 0：右压 |Δ|，左不动；
/// * |Δ| 不超过 [BALANCE_GAIN_MAX_DB]，小于 [BALANCE_GAIN_DEADBAND_DB] 时视为已平衡。
///
/// 幂等：增益只由 Δ 决定，Δ 由校准数据与端点标量决定，和当前增益无关，所以重复调用结果不变。
pub fn balance_gains(delta_db: f32) -> [f32; 2] {
    if !delta_db.is_finite() {
        return [0.0, 0.0];
    }
    let delta = delta_db.clamp(-BALANCE_GAIN_MAX_DB, BALANCE_GAIN_MAX_DB);
    if delta.abs() <= BALANCE_GAIN_DEADBAND_DB {
        return [0.0, 0.0];
    }
    if delta > 0.0 {
        [-delta, 0.0]
    } else {
        [0.0, delta]
    }
}

/// 校准过程的一条阶段说明（给界面/命令行显示进度用）。
#[derive(Clone, Debug, PartialEq)]
pub struct BalanceProgress {
    /// 当前阶段的中文说明。
    pub message: String,
    /// 已完成的小步数（曲线扫描算 2 步，每个锚点算 1 步）。
    pub done: usize,
    /// 总步数。
    pub total: usize,
}

/// 一次完整校准的结果：校准数据 + 过程信息（失败时也要能恢复音量）。
#[derive(Clone, Debug)]
pub struct BalanceCalibrationOutcome {
    /// 校准数据（曲线 + 锚点 + 灵敏度差）。
    pub calibration: BalanceCalibration,
    /// 校准前的左端点音量，出错时用来恢复。
    pub left_before: Option<EndpointVolume>,
    /// 校准前的右端点音量。
    pub right_before: Option<EndpointVolume>,
    /// 非致命问题（例如端点不支持曲线扫描、某个锚点信噪比偏低）。
    pub warnings: Vec<String>,
    /// 最后一个锚点的原始测量报告（界面展示实测 dB 用）。
    pub last_report: Option<BalanceReport>,
}

/// 一次性完成「端点曲线扫描 + 多锚点声学测量」的不带进度回调版本。
///
/// restore_scalar：校准结束后把两端写回的标量。
/// * Some(s)：写回 s（跟随生效时传主音量，保持「两端 = 系统音量」的不变量）；
/// * None：恢复校准前的原值。
///
/// 中途任何一步失败都会先恢复校准前的音量再返回错误——绝不能让用户的两台音箱停在 90%
/// 的校准锚点上。
pub fn calibrate_balance(
    mic: &MicCandidate,
    left_endpoint: &str,
    right_endpoint: &str,
    cfg: &SyncConfig,
    anchor_scalars: &[f32],
    restore_scalar: Option<f32>,
) -> Result<BalanceCalibrationOutcome> {
    calibrate_balance_with_progress(
        mic,
        left_endpoint,
        right_endpoint,
        cfg,
        anchor_scalars,
        restore_scalar,
        |_| {},
    )
}

/// [calibrate_balance] 的带进度回调版本：每进入一个阶段回调一次 [BalanceProgress]。
///
/// 顺序：扫左端点曲线 → 扫右端点曲线 → 逐个锚点「设标量 + 测左右电平」→ 恢复音量。
pub fn calibrate_balance_with_progress(
    mic: &MicCandidate,
    left_endpoint: &str,
    right_endpoint: &str,
    cfg: &SyncConfig,
    anchor_scalars: &[f32],
    restore_scalar: Option<f32>,
    mut progress: impl FnMut(&BalanceProgress),
) -> Result<BalanceCalibrationOutcome> {
    let left_endpoint = left_endpoint.trim();
    let right_endpoint = right_endpoint.trim();
    if left_endpoint.is_empty() || right_endpoint.is_empty() {
        return Err(AudioError::invalid("左右端点都不能为空").into());
    }
    let mut anchors: Vec<f32> = anchor_scalars
        .iter()
        .copied()
        .filter(|s| s.is_finite())
        .map(|s| s.clamp(0.02, 1.0))
        .collect();
    if anchors.is_empty() {
        anchors = BALANCE_ANCHOR_SCALARS.to_vec();
    }
    anchors.sort_by(|a, b| a.total_cmp(b));
    anchors.dedup_by(|a, b| (*a - *b).abs() < 1e-4);

    let left_before = get_endpoint_volume(left_endpoint).ok();
    let right_before = get_endpoint_volume(right_endpoint).ok();
    let mut warnings = Vec::new();

    // 失败/提前返回（?）时由 Drop 恢复音量；成功收尾后 disarm。
    let mut guard = RestoreGuard {
        left: left_endpoint.to_string(),
        right: right_endpoint.to_string(),
        left_before,
        right_before,
        armed: true,
    };

    let total = 2 + anchors.len();
    let mut done = 0usize;
    let step = |progress: &mut dyn FnMut(&BalanceProgress), message: String, done: usize| {
        progress(&BalanceProgress {
            message,
            done,
            total,
        });
    };

    // 1) 端点曲线：纯软件扫描（先静音、结束恢复），扫不到就回退典型曲线并记一条警告。
    step(&mut progress, "扫描左端点音量曲线…".to_string(), done);
    let left_curve = match capture_endpoint_curve(left_endpoint) {
        Ok(points) => EndpointCurve::new(points),
        Err(e) => {
            warnings.push(format!("左端点曲线扫描失败（改用典型曲线）：{e}"));
            EndpointCurve::default()
        }
    };
    done += 1;

    step(&mut progress, "扫描右端点音量曲线…".to_string(), done);
    let right_curve = match capture_endpoint_curve(right_endpoint) {
        Ok(points) => EndpointCurve::new(points),
        Err(e) => {
            warnings.push(format!("右端点曲线扫描失败（改用典型曲线）：{e}"));
            EndpointCurve::default()
        }
    };
    done += 1;

    // 2) 声学锚点：先把两端写到同一标量，再量左右电平差。
    let mut measured: Vec<BalanceAnchor> = Vec::with_capacity(anchors.len());
    let mut last_report = None;
    for (index, anchor) in anchors.iter().copied().enumerate() {
        step(
            &mut progress,
            format!(
                "锚点 {}/{}：把两台音箱写到 {:.0}% 并播放测试音…",
                index + 1,
                anchors.len(),
                anchor * 100.0
            ),
            done,
        );
        set_endpoint_volume(left_endpoint, anchor)
            .with_context(|| format!("把左端点写到锚点标量 {anchor:.2} 失败"))?;
        set_endpoint_volume(right_endpoint, anchor)
            .with_context(|| format!("把右端点写到锚点标量 {anchor:.2} 失败"))?;
        // 驱动可能把标量量化到固定档位，读回真实值再算曲线差。
        let actual_left = get_endpoint_volume(left_endpoint)
            .map(|v| v.scalar)
            .unwrap_or(anchor);
        let actual_right = get_endpoint_volume(right_endpoint)
            .map(|v| v.scalar)
            .unwrap_or(anchor);
        let master = ((actual_left + actual_right) * 0.5).clamp(0.0, 1.0);

        let report = measure_balance(mic, left_endpoint, right_endpoint, cfg)
            .with_context(|| format!("锚点 {:.0}% 测量失败", anchor * 100.0))?;
        let confidence = report.confidence as f32;
        if confidence < 0.2 {
            warnings.push(format!(
                "锚点 {:.0}% 信噪比偏低（可信度 {:.0}%），这次校准可能不准，建议环境安静后重测",
                anchor * 100.0,
                confidence * 100.0
            ));
        }
        measured.push(BalanceAnchor {
            master,
            master_db: taper_db(master) as f32,
            delta_db: report.delta_db as f32,
            left_db: report.left_db as f32,
            right_db: report.right_db as f32,
            confidence,
        });
        last_report = Some(report);
        done += 1;
    }
    measured.sort_by(|a, b| a.master.total_cmp(&b.master));

    // 3) 汇总：灵敏度差取满音量处（锚点外推）的值，便于展示与兜底。
    let mut calibration = BalanceCalibration {
        version: BALANCE_CAL_VERSION,
        left_endpoint: left_endpoint.to_string(),
        right_endpoint: right_endpoint.to_string(),
        anchors: measured,
        left_curve,
        right_curve,
        sens_diff_db: 0.0,
        calibrated_at: unix_now(),
    };
    calibration.sens_diff_db = calibration.sens_at(1.0);

    // 4) 收尾：写回主音量（跟随生效）或恢复原值。
    step(&mut progress, "恢复音量…".to_string(), total);
    match restore_scalar {
        Some(scalar) => {
            set_endpoint_volume(left_endpoint, scalar.clamp(0.0, 1.0))
                .context("收尾写回左端点音量失败")?;
            set_endpoint_volume(right_endpoint, scalar.clamp(0.0, 1.0))
                .context("收尾写回右端点音量失败")?;
            restore_mute(left_endpoint, left_before.as_ref());
            restore_mute(right_endpoint, right_before.as_ref());
        }
        None => {
            restore_endpoint(left_endpoint, left_before.as_ref());
            restore_endpoint(right_endpoint, right_before.as_ref());
        }
    }
    guard.armed = false;

    Ok(BalanceCalibrationOutcome {
        calibration,
        left_before,
        right_before,
        warnings,
        last_report,
    })
}

/// 失败路径上「把两端音量恢复原值」的守卫：Drop 时（未 disarm）执行恢复。
struct RestoreGuard {
    left: String,
    right: String,
    left_before: Option<EndpointVolume>,
    right_before: Option<EndpointVolume>,
    armed: bool,
}

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        restore_endpoint(&self.left, self.left_before.as_ref());
        restore_endpoint(&self.right, self.right_before.as_ref());
    }
}

/// 把端点音量与静音状态恢复成校准前的样子（失败只记日志，不覆盖原始错误）。
fn restore_endpoint(selector: &str, before: Option<&EndpointVolume>) {
    let Some(before) = before else {
        return;
    };
    if let Err(e) = set_endpoint_volume(selector, before.scalar) {
        tracing::warn!("[平衡校准] 恢复「{selector}」音量失败：{e}");
    }
    restore_mute(selector, Some(before));
}

/// 只恢复静音状态。
fn restore_mute(selector: &str, before: Option<&EndpointVolume>) {
    let Some(before) = before else {
        return;
    };
    if let Err(e) = set_endpoint_mute(selector, before.muted) {
        tracing::warn!("[平衡校准] 恢复「{selector}」静音状态失败：{e}");
    }
}

/// 当前 Unix 秒（拿不到系统时间就当 0）。
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 在一组按 x 升序的点上分段线性插值；超出两端按端点值外推。
fn interp(points: &[(f32, f32)], x: f32) -> f32 {
    debug_assert!(!points.is_empty());
    if points.len() == 1 || x <= points[0].0 {
        return points[0].1;
    }
    let last = points[points.len() - 1];
    if x >= last.0 {
        return last.1;
    }
    for pair in points.windows(2) {
        let (x0, y0) = pair[0];
        let (x1, y1) = pair[1];
        if x >= x0 && x <= x1 {
            let span = x1 - x0;
            if span.abs() < 1e-9 {
                return y1;
            }
            return y0 + (y1 - y0) * (x - x0) / span;
        }
    }
    last.1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve(points: &[(f32, f32)]) -> EndpointCurve {
        EndpointCurve::new(points.to_vec())
    }

    fn anchor(master: f32, delta_db: f32) -> BalanceAnchor {
        BalanceAnchor {
            master,
            master_db: taper_db(master) as f32,
            delta_db,
            left_db: -20.0 + delta_db,
            right_db: -20.0,
            confidence: 0.9,
        }
    }

    fn calibration_with(anchors: Vec<BalanceAnchor>) -> BalanceCalibration {
        BalanceCalibration {
            anchors,
            ..BalanceCalibration::default()
        }
    }

    #[test]
    fn curve_sorts_clamps_and_dedups() {
        let c = curve(&[
            (1.0, 0.0),
            (0.5, -10.3),
            (0.5, -99.0),
            (1.5, -4.0),
            (f32::NAN, -1.0),
            (-0.2, -70.0),
        ]);
        let scalars: Vec<f32> = c.points.iter().map(|(s, _)| *s).collect();
        assert_eq!(scalars, vec![0.0, 0.5, 1.0], "{scalars:?}");
        // 1.5 被夹成 1.0，和原本的 (1.0, 0.0) 冲突时保留先出现的那个。
        assert_eq!(c.points[2], (1.0, 0.0));
    }

    #[test]
    fn curve_interpolates_and_extrapolates_flat() {
        let c = curve(&[(0.5, -10.3), (1.0, 0.0)]);
        assert!((c.db_at(0.75) + 5.15).abs() < 1e-3, "{}", c.db_at(0.75));
        assert!((c.db_at(0.2) + 10.3).abs() < 1e-3);
        assert!((c.db_at(1.0) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn empty_curve_falls_back_to_taper() {
        let c = EndpointCurve::default();
        assert!(c.is_empty());
        assert!((c.db_at(0.5) - taper_db(0.5) as f32).abs() < 1e-6);
        assert!((c.db_at(0.0) - taper_db(0.0) as f32).abs() < 1e-6);
    }

    #[test]
    fn gains_only_attenuate_the_louder_side() {
        assert_eq!(balance_gains(6.0), [-6.0, 0.0]);
        // Δ = -6（右边更响）→ 右路压 6 dB，即 -6.0（增益一律 ≤ 0）。
        assert_eq!(balance_gains(-6.0), [0.0, -6.0]);
        assert_eq!(balance_gains(0.0), [0.0, 0.0]);
        assert_eq!(balance_gains(0.1), [0.0, 0.0]);
        assert_eq!(balance_gains(f32::NAN), [0.0, 0.0]);
        // 上限 24 dB：更响的那路最多压 24 dB，安静那路永远 0 dB。
        assert_eq!(balance_gains(100.0), [-BALANCE_GAIN_MAX_DB, 0.0]);
        assert_eq!(balance_gains(-100.0), [0.0, -BALANCE_GAIN_MAX_DB]);
        assert_eq!(balance_gains(-100.0), [0.0, -24.0]);
        let gains = balance_gains(3.0);
        assert!(gains.iter().all(|g| *g <= 0.0), "{gains:?}");
    }

    #[test]
    fn residual_is_within_deadband_after_applying_gains() {
        for delta in [-30.0f32, -7.5, -1.0, 0.0, 1.0, 7.5, 30.0] {
            let gains = balance_gains(delta);
            let residual = delta + gains[0] - gains[1];
            if delta.abs() <= BALANCE_GAIN_DEADBAND_DB {
                assert_eq!(gains, [0.0, 0.0]);
                assert!(residual.abs() <= BALANCE_GAIN_DEADBAND_DB);
            } else {
                // 超出 24 dB 上限时，剩下的部分（|delta| - 24）压不掉，残差允许到那里。
                let excess = (delta.abs() - BALANCE_GAIN_MAX_DB).max(0.0);
                assert!(
                    residual.abs() <= BALANCE_GAIN_DEADBAND_DB.max(excess) + 1e-4,
                    "delta={delta} residual={residual}"
                );
            }
        }
    }

    #[test]
    fn gains_are_idempotent() {
        let cal = calibration_with(vec![anchor(0.6, 6.0)]);
        let first = cal.gains_following(0.6);
        let second = cal.gains_following(0.6);
        assert_eq!(first, second);
        assert_eq!(first, [-6.0, 0.0]);
    }

    #[test]
    fn sensitivity_is_flat_across_master_when_curves_match() {
        let cal = calibration_with(vec![anchor(0.6, 6.0)]);
        // 曲线都没扫（都回退典型曲线）→ 端点差为 0，Δ 应恒等于锚点的 6 dB。
        for master in [0.1f32, 0.3, 0.6, 0.9, 1.0] {
            let delta = cal.delta_at(master, master, master);
            assert!((delta - 6.0).abs() < 1e-3, "master={master} delta={delta}");
            assert_eq!(cal.gains_following(master), [-6.0, 0.0]);
        }
    }

    #[test]
    fn curve_difference_enters_delta() {
        // 没有锚点时灵敏度差回退 0，Δ 只剩端点曲线差。
        let cal = BalanceCalibration {
            left_curve: curve(&[(0.2, -23.6), (0.5, -10.3), (1.0, 0.0)]),
            right_curve: curve(&[(0.2, -23.6), (0.5, -7.3), (1.0, 0.0)]),
            ..BalanceCalibration::default()
        };
        // 同一标量下右端点高了 3 dB → Δ = -3 dB（左边更安静），应当压右路 3 dB。
        let delta = cal.delta_at(0.5, 0.5, 0.5);
        assert!((delta + 3.0).abs() < 1e-3, "{delta}");
        let gains = cal.gains_at(0.5, 0.5, 0.5);
        assert!(
            (gains[0] - 0.0).abs() < 1e-6 && (gains[1] + 3.0).abs() < 1e-3,
            "{gains:?}"
        );
        assert!(cal.residual_db(0.5, 0.5, 0.5).abs() <= BALANCE_GAIN_DEADBAND_DB);
    }

    #[test]
    fn sensitivity_interpolates_between_anchors() {
        let cal = calibration_with(vec![anchor(0.6, 6.0), anchor(0.9, 9.0)]);
        let low = cal.sens_at(0.6);
        let mid = cal.sens_at(0.75);
        let high = cal.sens_at(0.9);
        assert!((low - 6.0).abs() < 1e-3, "{low}");
        assert!((high - 9.0).abs() < 1e-3, "{high}");
        assert!(mid > low && mid < high, "mid={mid} low={low} high={high}");
    }

    #[test]
    fn empty_calibration_is_neutral() {
        let cal = BalanceCalibration::default();
        assert!(cal.is_empty());
        assert_eq!(cal.gains_following(0.7), [0.0, 0.0]);
        assert_eq!(cal.describe(), "尚未校准");
    }

    /// 这个 bug 的回归测试：跟随系统音量时两端标量都被镜像成主音量 m，
    /// 所以要证明「任意 m 下残差都落在死区内」——平衡不能因为调音量而回来。
    ///
    /// 同时证明补偿量随 m 变化（不是固定百分比差）：两端点曲线不同，
    /// 低音量段需要的补偿和高音量段差 > 0.5 dB。
    #[test]
    fn following_keeps_balance_across_master_sweep() {
        // 两端曲线都从典型 taper 出发，但右端点在低音量段多衰减（模拟 3.4 dB 的曲线差）。
        let left = curve(&[
            (0.05, -41.6),
            (0.2, -23.6),
            (0.5, -10.3),
            (0.9, -1.6),
            (1.0, 0.0),
        ]);
        let right = curve(&[
            (0.05, -45.0),
            (0.2, -25.1),
            (0.5, -10.3),
            (0.9, -1.6),
            (1.0, 0.0),
        ]);
        // 真实锚点：实测差 = 端点曲线差 + 上面板灵敏度差（这里是 2 dB，左更响）。
        let sens = 2.0f32;
        let anchors: Vec<BalanceAnchor> = [0.30f32, 0.60, 0.90]
            .into_iter()
            .map(|m| {
                let delta = left.db_at(m) - right.db_at(m) + sens;
                BalanceAnchor {
                    master: m,
                    master_db: taper_db(m) as f32,
                    delta_db: delta,
                    left_db: -20.0 + delta,
                    right_db: -20.0,
                    confidence: 0.9,
                }
            })
            .collect();
        let cal = BalanceCalibration {
            anchors,
            left_curve: left,
            right_curve: right,
            ..BalanceCalibration::default()
        };

        let mut gains_at_low = [0.0f32; 2];
        for master in [0.05f32, 0.1, 0.2, 0.3, 0.45, 0.6, 0.75, 0.9, 1.0] {
            let residual = cal.residual_db(master, master, master);
            assert!(
                residual.abs() <= BALANCE_GAIN_DEADBAND_DB + 1e-3,
                "master={master} residual={residual}"
            );
            // 补偿只压不抬，且「较安静的一侧」永远是 0 dB 基准。
            let gains = cal.gains_following(master);
            assert!(
                gains.iter().all(|g| *g <= 0.0),
                "master={master} gains={gains:?}"
            );
            assert!(
                gains[0] == 0.0 || gains[1] == 0.0,
                "master={master} gains={gains:?} 不该两侧同时压"
            );
            if (master - 0.05).abs() < 1e-6 {
                gains_at_low = gains;
            }
        }
        // 极低音量的补偿量与满音量不同 → 证明必须按当前主音量重算，而不是存死百分比差。
        let gains_at_high = cal.gains_following(1.0);
        let diff =
            (gains_at_low[0] - gains_at_high[0]).abs() + (gains_at_low[1] - gains_at_high[1]).abs();
        assert!(
            diff > 0.5,
            "低/高音量补偿量应当不同：{gains_at_low:?} vs {gains_at_high:?}"
        );
    }

    #[test]
    fn describe_mentions_anchor_and_sens() {
        let cal = calibration_with(vec![anchor(0.6, 6.0)]);
        let text = cal.describe();
        assert!(text.contains("1 个锚点"), "{text}");
        assert!(text.contains("灵敏度差"), "{text}");
    }
}
