//! balance_probe —— 左右音箱音量平衡探针 / 校准（task-22 + balance 模块）。
//!
//! 两种工作模式：
//!
//! * 旧模式（不给 --calibrate）：单次测量左右电平差，只给「把端点音量调成
//!   百分之几」的建议（--apply 时写回端点音量）。
//! * 校准模式（给 --calibrate）：跑整轮平衡校准——扫两只端点的「标量 → dB」曲线 +
//!   在多个主音量锚点上实测左右电平差，最后推导出喇叭灵敏度差。结果是一份
//!   BalanceCalibration，运行时由路由引擎按 Δ 给较响的一路做**数字衰减**（只压不抬），
//!   所以之后用户调系统音量时平衡不会被破坏。
//!
//! 用法示例：
//!
//!     # 1) 先列出端点，抄名字 / ID 子串
//!     cargo run -p stereo-audio --example balance_probe -- --list
//!
//!     # 2) 旧模式：单次测量（只打印建议，不改设置）
//!     cargo run -p stereo-audio --example balance_probe -- --mic "麦克风" --left "音箱 A" --right "音箱 B"
//!
//!     # 2b) 旧模式 + 写回建议的端点音量
//!     cargo run -p stereo-audio --example balance_probe -- --mic "麦克风" --left "音箱 A" --right "音箱 B" --apply
//!
//!     # 3) 校准模式：整轮校准（默认锚点 0.30 / 0.60 / 0.90）
//!     cargo run -p stereo-audio --example balance_probe -- --calibrate --mic "麦克风" --left "左端点" --right "右端点" --amplitude 0.6
//!
//!     # 3b) 自定义锚点，并把两端音量写回 60%
//!     cargo run -p stereo-audio --example balance_probe -- --calibrate --mic "麦克风" --left "左端点" --right "右端点" --anchors 0.3,0.6,0.9 --restore 0.6
//!
//! ## 通用参数
//!
//! * --list：列出本机采集端点（麦克风候选）与 render 端点，方便抄名字。
//! * --mic <子串>：麦克风（采集端点）名字或 ID 子串。
//! * --left <端点> / --right <端点>：左右 render 端点，名字或 ID 子串。
//!   端点支持名字子串、完整端点 id（{0.0.0.00000000}.{...}）或**唯一 id 片段**
//!   （例如 9809a2e0）。遇到两个同名变体时用唯一 id 片段最稳。
//! * --probe-ms <n>：测试音时长，默认 200 ms（夹到 200~2000）。麦克风 AGC / 降噪
//!   需要时间稳定，实测传 600 更稳（锚点重复性明显更好）。
//! * --amplitude <0..1>：测试音幅度，默认 0.8；自己听感吵就降到 0.6 左右。
//!
//! ## 旧模式
//!
//! **只**往 --left 端点放一段 1 kHz 测试音，用 --mic 独占模式录下来量电平；
//! 再**只**往 --right 放同一段，量电平。两路分开放、分开测，避免串音把两路混淆。
//! 然后用 measure_balance 把两路电平差换算成「建议把哪个端点的音量调成百分之几」：
//! 较响的一路为基准不动，抬较安静的一路；抬到 100% 仍不够就改把较响的一路压下来。
//! --apply 时把建议值用 set_endpoint_volume 真正写回两个端点（写后读回校验），
//! 不加时**只打印建议、不改任何设置**。
//!
//! 注意：这条路改的是端点音量，而音量联动（apply_follow）会把采集源的标量原样镜像到
//! 每台目标设备，两台的标量随时会被写成同一个值——平衡信息只存在于「两个标量之差」里，
//! 是结构性的易失。所以真正要长期保持平衡请用校准模式。
//!
//! ## 校准模式（--calibrate）
//!
//! 调用 calibrate_balance_with_progress 一次做全：
//!
//! 1. **扫左/右端点曲线**：纯软件逐点写 SetMasterVolumeLevelScalar + 读
//!    GetMasterVolumeLevel，先静音、结束恢复，用户听不到；拿到两只设备真实的
//!    「标量 → dB」曲线（不是典型曲线）。
//! 2. **声学锚点**：把两端写到同一标量，各放一次左右测试音，实测电平差 delta_db。
//!    默认锚点 BALANCE_ANCHOR_SCALARS = [0.30, 0.60, 0.90]（小/中/大音量）。
//! 3. **推导灵敏度差 sens(m)**：delta_k − (curve_L(m_k) − curve_R(m_k))，
//!    锚点之间按 dB 线性插值。
//!
//! 校准模式参数：
//!
//! * --anchors <a,b,c>：校准锚点（主音量标量，逗号分隔，0..1）。不给就用
//!   BALANCE_ANCHOR_SCALARS。
//! * --restore <0..1>：校准结束后把**两端**写回的标量（只接受数字）。
//!   不给 = 恢复校准前的原值。跟随生效时通常传当前主音量（例如 --restore 0.6）。
//! * --json <路径>：把校准结果写成 stereo-link config.json 里 balance 字段的 JSON，
//!   便于直接注入。默认 E:\AI\MiPlay\.scratch\balance\calibration.json（父目录自动创建）。
//!
//! 校准完成后打印：calibration.describe()、每个锚点一行（anchor_label + 原始 dBFS 与
//! master）、左右端点曲线点数与首尾点、sens_diff_db、**验证表**（对 master ∈
//! [0.05, 0.1, 0.2, 0.3, 0.5, 0.7, 0.9, 1.0] 打印 delta_at(m,m,m) /
//! gains_following(m) / residual_db(m,m,m)，另加一张「手动左 0.8 / 右 0.5」组合表证明
//! 手动音量也能算）、warnings、以及音量是否已恢复；最后还会写出一份校准 JSON
//! （--json，默认 .scratch/balance/calibration.json）。
//!
//! 补偿规则：Δ = curve_L(s_L) − curve_R(s_R) + sens(m)，
//! g_L = min(−Δ, 0)、g_R = min(Δ, 0)——只压不抬、上限 24 dB、死区 0.2 dB。
//! 因此应用补偿后 residual_db 应当落在 ±0.2 dB 内；只有 |Δ| > 24 dB 的场合才会残留
//! 压不掉的部分（本程序会显式标 ⚠ 并说明原因）。

use std::error::Error;
use std::fs;
use std::io::Write;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use stereo_audio::balance::{
    calibrate_balance_with_progress, BalanceCalibration, BalanceProgress, EndpointCurve,
    BALANCE_ANCHOR_SCALARS, BALANCE_CAL_VERSION, BALANCE_GAIN_DEADBAND_DB, BALANCE_GAIN_MAX_DB,
};
use stereo_audio::sync::{list_microphones, measure_balance, MicCandidate, SyncConfig};
use stereo_audio::volume::{
    get_endpoint_volume, set_endpoint_volume, EndpointVolume, CURVE_SCALARS,
};
use stereo_audio::{list_endpoints, DataFlow};

/// 校准验证表使用的主音量档位（覆盖很小到满音量）。
const VERIFY_MASTERS: [f32; 8] = [0.05, 0.1, 0.2, 0.3, 0.5, 0.7, 0.9, 1.0];

/// 「用户手动把左路改到 80%、右路改到 50%」的组合（验证手动音量也能算 Δ）。
const MANUAL_COMBO: (f32, f32) = (0.8, 0.5);

/// 校准 JSON 的默认输出路径（stereo-link config.json 里 `balance` 字段的形状）。
const DEFAULT_JSON_PATH: &str = r"E:\AI\MiPlay\.scratch\balance\calibration.json";

/// --key value 取值；key 也可以写成 --key=value。
fn arg_value(args: &[String], key: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == key {
            return it.next().cloned();
        }
        if let Some(rest) = a.strip_prefix(&format!("{key}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

/// 命令行里有没有这个开关。
fn has_flag(args: &[String], key: &str) -> bool {
    args.iter().any(|a| a == key)
}

/// 按名字/ID 子串挑一个采集端点。
fn pick_mic(spec: &str) -> Result<MicCandidate, Box<dyn Error>> {
    let all = list_microphones();
    if all.is_empty() {
        return Err("本机没有任何采集端点".into());
    }
    let lower = spec.to_ascii_lowercase();
    let hits: Vec<&MicCandidate> = all
        .iter()
        .filter(|m| {
            m.name().to_ascii_lowercase().contains(&lower)
                || m.id().to_ascii_lowercase().contains(&lower)
        })
        .collect();
    match hits.first() {
        Some(m) => {
            if hits.len() > 1 {
                let names: Vec<String> = hits.iter().map(|h| h.name().to_string()).collect();
                println!(
                    "⚠ 「{spec}」匹配到 {} 个采集端点（{}），按第一个处理；建议用唯一 id 片段（例如 08061fc4）区分",
                    hits.len(),
                    names.join(" / ")
                );
            }
            println!("麦克风：{}", m.describe());
            Ok((*m).clone())
        }
        None => {
            let names: Vec<String> = all.iter().map(|m| m.name().to_string()).collect();
            Err(format!("找不到麦克风「{spec}」；可用：{}", names.join(" / ")).into())
        }
    }
}

/// 打印某个端点当前的音量（EndpointVolume::describe()）。
fn print_current(tag: &str, spec: &str) {
    match get_endpoint_volume(spec) {
        Ok(v) => println!("  {tag} {spec} -> {}", v.describe()),
        Err(e) => println!("  {tag} {spec} -> 读不到音量：{e}"),
    }
}

/// 解析 --anchors：逗号分隔的 0..1 标量，排序去重。
fn parse_anchors(spec: &str) -> Result<Vec<f32>, String> {
    let mut anchors = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let scalar = part
            .parse::<f32>()
            .map_err(|_| format!("--anchors 里的「{part}」不是数字"))?;
        if !scalar.is_finite() || !(0.0..=1.0).contains(&scalar) {
            return Err(format!("--anchors 里的「{part}」不在 0..1 范围"));
        }
        anchors.push(scalar);
    }
    if anchors.is_empty() {
        return Err("--anchors 至少要给一个 0..1 的标量".to_string());
    }
    anchors.sort_by(|a, b| a.total_cmp(b));
    anchors.dedup_by(|a, b| (*a - *b).abs() < 1e-4);
    Ok(anchors)
}

/// 解析 --restore：只接受 0..1 的数字。
fn parse_restore(spec: &str) -> Result<f32, String> {
    let scalar = spec.trim().parse::<f32>().map_err(|_| {
        format!("--restore 只接受 0..1 的数字（例如 --restore 0.6）；收到「{spec}」")
    })?;
    if !scalar.is_finite() || !(0.0..=1.0).contains(&scalar) {
        return Err(format!("--restore 必须在 0..1 之间；收到「{spec}」"));
    }
    Ok(scalar)
}

/// 把 --left/--right 的端点选择器解析成完整端点 id。
///
/// 底层 volume / sync 的选择器只认完整 `{0.0...}` id 或**名字**子串，不认 id 片段；
/// 这里额外支持唯一的 id 片段（例如 9809a2e0），并统一换成完整 id 传下去，
/// 免得两台同名 Xiaomi 音箱靠名字先到先得。返回 (显示名, 完整 id)。
fn resolve_render(spec: &str) -> Result<(String, String), Box<dyn Error>> {
    let endpoints: Vec<_> = list_endpoints()?
        .into_iter()
        .filter(|e| e.flow == DataFlow::Render)
        .collect();
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("--left / --right 不能是空串".into());
    }
    if spec.starts_with('{') {
        if let Some(hit) = endpoints.iter().find(|e| e.id.eq_ignore_ascii_case(spec)) {
            return Ok((hit.name.clone(), hit.id.clone()));
        }
        // 完整 id 但本机没枚举到：原样交给底层，由它给出带候选列表的错误。
        return Ok((spec.to_string(), spec.to_string()));
    }
    let lower = spec.to_ascii_lowercase();
    let id_hits: Vec<_> = endpoints
        .iter()
        .filter(|e| e.id.to_ascii_lowercase().contains(&lower))
        .collect();
    match id_hits.len() {
        1 => {
            let hit = id_hits[0];
            return Ok((hit.name.clone(), hit.id.clone()));
        }
        0 => {}
        _ => {
            let names: Vec<String> = id_hits
                .iter()
                .map(|e| format!("{} [{}]", e.name, e.id))
                .collect();
            return Err(format!(
                "「{spec}」匹配到多个 render 端点：{}；请给更长的片段或完整 id",
                names.join(" / ")
            )
            .into());
        }
    }
    let name_hits: Vec<_> = endpoints
        .iter()
        .filter(|e| e.name.to_ascii_lowercase().contains(&lower))
        .collect();
    match name_hits.first() {
        Some(hit) => {
            if name_hits.len() > 1 {
                let names: Vec<String> = name_hits
                    .iter()
                    .map(|e| format!("{} [{}]", e.name, e.id))
                    .collect();
                println!(
                    "⚠ 「{spec}」匹配到 {} 个 render 端点（{}），按第一个处理；建议改用唯一 id 片段",
                    name_hits.len(),
                    names.join(" / ")
                );
            }
            Ok((hit.name.clone(), hit.id.clone()))
        }
        None => {
            let names: Vec<String> = endpoints
                .iter()
                .map(|e| format!("{} [{}]", e.name, e.id))
                .collect();
            Err(format!(
                "找不到匹配的 render 端点「{spec}」；本机候选：{}",
                names.join(" / ")
            )
            .into())
        }
    }
}

/// 头部用法。
fn print_usage() {
    println!("用法：");
    println!("  balance_probe --list");
    println!(
        "  balance_probe --mic <子串> --left <端点> --right <端点> [--apply] [--probe-ms <n>] [--amplitude <0..1>]"
    );
    println!(
        "  balance_probe --calibrate --mic <子串> --left <端点> --right <端点> [--anchors 0.3,0.6,0.9] [--restore <0..1>] [--json <路径>] [--probe-ms <n>] [--amplitude <0..1>]"
    );
    println!();
    println!("端点参数支持名字子串、完整 ID（{{0.0.0.00000000}}.{{...}}）或唯一 ID 片段（如 9809a2e0）。");
    println!("--calibrate  整轮校准：扫端点曲线 + 多锚点声学测量，打印验证表。");
    println!("--anchors    校准锚点（主音量标量，逗号分隔，0..1；默认 0.3,0.6,0.9）。");
    println!("--restore    校准后把两端写回的标量（0..1）；不给则恢复校准前原值。");
    println!(
        "--json       校准结果 JSON 输出路径（默认 E:\\AI\\MiPlay\\.scratch\\balance\\calibration.json）。"
    );
    println!("--apply      仅旧模式：把单次测量建议的端点音量真正写回。");
}

/// --list：列出采集与 render 端点。
fn list_mode() -> Result<(), Box<dyn Error>> {
    println!("=== 采集端点（麦克风候选）===");
    for m in list_microphones() {
        println!("  {}", m.describe());
    }
    println!("=== render 端点 ===");
    for e in list_endpoints()? {
        if e.flow == DataFlow::Render {
            println!("  {} [{}]", e.name, e.id);
        }
    }
    Ok(())
}

/// 旧模式：单次测量左右电平差（--apply 时写回端点音量）。
fn measure_mode(
    args: &[String],
    mic_spec: &str,
    left: &str,
    right: &str,
    cfg: &SyncConfig,
) -> Result<(), Box<dyn Error>> {
    let apply = has_flag(args, "--apply");
    if arg_value(args, "--anchors").is_some() || arg_value(args, "--restore").is_some() {
        println!("（--anchors / --restore 只在 --calibrate 模式下生效，本次忽略）");
    }

    println!("=== 左右音量平衡测量 ===");
    println!(
        "  配置：测试音 {:.0} Hz，{} ms，幅度 {:.2}；录音窗口 {} ms（前置 {} + 测试 {} + 尾部 {}）",
        stereo_audio::sync::BALANCE_TONE_HZ,
        cfg.probe_ms,
        cfg.amplitude,
        cfg.window_ms(),
        cfg.pre_roll_ms,
        cfg.probe_ms,
        cfg.tail_ms
    );
    let mic = pick_mic(mic_spec)?;
    print_current("调整前", left);
    print_current("调整前", right);

    let started = Instant::now();
    let report = measure_balance(&mic, left, right, cfg)?;
    println!(
        "\n--- 结果（耗时 {:.1} s）---",
        started.elapsed().as_secs_f64()
    );
    println!("  {}", report.describe());
    println!(
        "  左 {:.2} dB / 右 {:.2} dB / delta {:+.2} dB / 置信度 {:.2}",
        report.left_db, report.right_db, report.delta_db, report.confidence
    );
    if report.balanced() {
        println!(
            "  两路相差 {:.2} dB，在 ±{:.1} dB 死区内 → 建议维持现状。",
            report.delta_db,
            stereo_audio::sync::BALANCE_DEADBAND_DB
        );
    } else {
        let louder = if report.delta_db > 0.0 { "左" } else { "右" };
        println!(
            "  {louder}路更响 {:+.2} dB → 建议 左 {}% / 右 {}%。",
            report.delta_db.abs(),
            report.suggested_left_percent,
            report.suggested_right_percent
        );
    }
    if report.confidence < 0.5 {
        println!(
            "  ⚠ 置信度只有 {:.2}：测试音信噪比偏低（房间太吵 / 麦克风太远 / 音量太小），这次建议别直接照抄。",
            report.confidence
        );
    }

    if apply {
        println!("\n--- 写回 ---");
        let l = set_endpoint_volume(left, report.suggested_left_percent as f32 / 100.0)?;
        println!("  左 {left} -> {}", l.describe());
        let r = set_endpoint_volume(right, report.suggested_right_percent as f32 / 100.0)?;
        println!("  右 {right} -> {}", r.describe());
        println!(
            "  已写回。建议**再跑一次**确认 delta 落进死区（端点音量曲线不是线性的，一次不一定到位）。"
        );
    } else {
        println!("\n  （只打印建议，未改动任何设置；要写回请加 --apply）");
    }
    Ok(())
}

/// 打印一条端点曲线（describe() + 首尾点 + 点数）。
fn print_curve(tag: &str, curve: &EndpointCurve) {
    println!("  {tag}端点曲线：{}", curve.describe());
    match (curve.points.first(), curve.points.last()) {
        (Some(first), Some(last)) => println!(
            "    首点 {:.3} → {:.2} dB，尾点 {:.3} → {:.2} dB，共 {} 点",
            first.0,
            first.1,
            last.0,
            last.1,
            curve.points.len()
        ),
        _ => println!("    ⚠ 没扫到点：db_at() 会回退典型曲线（VOLUME_TAPER），灵敏度差可能偏大"),
    }
}

/// 给一行验证数据打判断：死区内 OK；残差 > 1 dB 显式标 ⚠ 并说明原因。
fn residual_note(delta_db: f32, residual_db: f32) -> String {
    if residual_db.abs() <= BALANCE_GAIN_DEADBAND_DB {
        "OK（死区内）".to_string()
    } else if residual_db.abs() > 1.0 {
        if delta_db.abs() > BALANCE_GAIN_MAX_DB {
            format!(
                "⚠ 残差超 1 dB：|Δ| {:.1} dB 超过 {:.0} dB 增益上限，压不动（剩下 {:+.1} dB）",
                delta_db.abs(),
                BALANCE_GAIN_MAX_DB,
                residual_db
            )
        } else {
            format!(
                "⚠ 残差超 1 dB：{:+.2} dB，检查校准数据/数值异常",
                residual_db
            )
        }
    } else {
        format!("△ 略超死区（{:+.2} dB）", residual_db)
    }
}

/// 验证表：跟随语义（两端标量 = 主音量）+ 手动音量组合。
fn print_verification(cal: &BalanceCalibration) {
    println!("\n--- 验证表 1：跟随语义（两端标量 = 主音量 m）---");
    println!(
        "  {:>6} | {:>8} | {:>8} | {:>7} | {:>7} | {:>9} | 判断",
        "master", "Δ dB", "sens dB", "g_L dB", "g_R dB", "residual"
    );
    let mut bad: Vec<f32> = Vec::new();
    for master in VERIFY_MASTERS {
        let delta = cal.delta_at(master, master, master);
        let gains = cal.gains_following(master);
        let residual = cal.residual_db(master, master, master);
        if residual.abs() > 1.0 {
            bad.push(master);
        }
        println!(
            "  {:>6.2} | {:>+8.2} | {:>+8.2} | {:>+7.2} | {:>+7.2} | {:>+9.2} | {}",
            master,
            delta,
            cal.sens_at(master),
            gains[0],
            gains[1],
            residual,
            residual_note(delta, residual)
        );
    }
    if bad.is_empty() {
        println!(
            "  结论：所有档位 residual 都在 ±{:.1} dB 死区内 → 补偿后左右应等响。",
            BALANCE_GAIN_DEADBAND_DB
        );
    } else {
        println!(
            "  ⚠ 结论：master = {:?} 的 residual 超过 1 dB（见上表原因）。",
            bad
        );
    }

    let (left_scalar, right_scalar) = MANUAL_COMBO;
    println!(
        "\n--- 验证表 2：手动音量组合（左 {:.0}% / 右 {:.0}%，证明手动改过某一路也能算）---",
        left_scalar * 100.0,
        right_scalar * 100.0
    );
    println!(
        "  {:>6} | {:>8} | {:>7} | {:>7} | {:>9} | 判断",
        "master", "Δ dB", "g_L dB", "g_R dB", "residual"
    );
    for master in VERIFY_MASTERS {
        let delta = cal.delta_at(left_scalar, right_scalar, master);
        let gains = cal.gains_at(left_scalar, right_scalar, master);
        let residual = cal.residual_db(left_scalar, right_scalar, master);
        println!(
            "  {:>6.2} | {:>+8.2} | {:>+7.2} | {:>+7.2} | {:>+9.2} | {}",
            master,
            delta,
            gains[0],
            gains[1],
            residual,
            residual_note(delta, residual)
        );
    }
}

/// 校准后检查端点音量是否已恢复（或已写回 --restore 给的值）。
fn print_restore_check(
    tag: &str,
    selector: &str,
    before: Option<&EndpointVolume>,
    expected: Option<f32>,
) {
    match get_endpoint_volume(selector) {
        Ok(now) => {
            println!("  {tag} {selector} -> {}", now.describe());
            match expected {
                Some(scalar) => {
                    if (now.scalar - scalar).abs() <= 0.02 {
                        println!("     ✅ 已按 --restore 写回 {scalar:.3}");
                    } else {
                        println!(
                            "     ⚠ 期望按 --restore 写回 {scalar:.3}，实际 {:.3}",
                            now.scalar
                        );
                    }
                }
                None => match before {
                    Some(before) => {
                        let mute_now = if now.muted { "静音" } else { "未静音" };
                        let scalar_ok = (now.scalar - before.scalar).abs() <= 0.02;
                        let mute_ok = now.muted == before.muted;
                        if scalar_ok && mute_ok {
                            println!(
                                "     ✅ 已恢复校准前原值（音量 {:.3}，{mute_now}）",
                                before.scalar
                            );
                        } else {
                            println!(
                                "     ⚠ 未恢复：校准前 {}，现在 {}",
                                before.describe(),
                                now.describe()
                            );
                        }
                    }
                    None => println!(
                        "     ⚠ 校准前读不到该端点音量，无法比对（当前 {}）",
                        now.describe()
                    ),
                },
            }
        }
        Err(e) => println!("  ⚠ {tag} {selector} 读不到当前音量：{e}"),
    }
}

/// 校准模式：整轮校准 + 详细验证输出。
/// 把 f32 写成 JSON 数字（非有限值降级成 0，避免产出非法 JSON）。
fn json_num(value: f32) -> String {
    if value.is_finite() {
        format!("{value}")
    } else {
        "0".to_string()
    }
}

/// JSON 字符串字面量（转义引号、反斜杠与控制字符）。
fn json_str(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 端点曲线点序列的 JSON 数组（每项 [scalar, db]，原样输出）。
fn curve_points_json(curve: &EndpointCurve) -> String {
    let points: Vec<String> = curve
        .points
        .iter()
        .map(|(scalar, db)| format!("[{}, {}]", json_num(*scalar), json_num(*db)))
        .collect();
    format!("[{}]", points.join(", "))
}

/// 把校准结果写成 stereo-link config.json 的 balance 字段形状（与它的
/// BalanceCalConfig / BalanceAnchorConfig 一一对应，可直接整段注入）。
fn write_calibration_json(cal: &BalanceCalibration, path: &str) -> Result<(), Box<dyn Error>> {
    let mut text = String::new();
    text.push_str("{\n");
    text.push_str(&format!("  \"version\": {},\n", cal.version));
    text.push_str(&format!(
        "  \"left_endpoint\": {},\n",
        json_str(&cal.left_endpoint)
    ));
    text.push_str(&format!(
        "  \"right_endpoint\": {},\n",
        json_str(&cal.right_endpoint)
    ));

    text.push_str("  \"anchors\": [");
    if cal.anchors.is_empty() {
        text.push_str("],\n");
    } else {
        text.push('\n');
        for (index, anchor) in cal.anchors.iter().enumerate() {
            text.push_str(&format!(
                "    {{\"master\": {}, \"master_db\": {}, \"delta_db\": {}, \"left_db\": {}, \"right_db\": {}, \"confidence\": {}}}",
                json_num(anchor.master),
                json_num(anchor.master_db),
                json_num(anchor.delta_db),
                json_num(anchor.left_db),
                json_num(anchor.right_db),
                json_num(anchor.confidence)
            ));
            if index + 1 < cal.anchors.len() {
                text.push(',');
            }
            text.push('\n');
        }
        text.push_str("  ],\n");
    }

    text.push_str(&format!(
        "  \"left_curve\": {},\n",
        curve_points_json(&cal.left_curve)
    ));
    text.push_str(&format!(
        "  \"right_curve\": {},\n",
        curve_points_json(&cal.right_curve)
    ));
    text.push_str(&format!(
        "  \"sens_diff_db\": {},\n",
        json_num(cal.sens_diff_db)
    ));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(cal.calibrated_at);
    text.push_str(&format!("  \"calibrated_at\": {now}\n"));
    text.push_str("}\n");

    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(path, text)?;
    Ok(())
}

fn calibrate_mode(
    args: &[String],
    mic_spec: &str,
    left: &str,
    right: &str,
    cfg: &SyncConfig,
) -> Result<(), Box<dyn Error>> {
    let anchors = match arg_value(args, "--anchors") {
        Some(spec) => parse_anchors(&spec)?,
        None => BALANCE_ANCHOR_SCALARS.to_vec(),
    };
    let restore = match arg_value(args, "--restore") {
        Some(spec) => Some(parse_restore(&spec)?),
        None => None,
    };
    let json_path = arg_value(args, "--json").unwrap_or_else(|| DEFAULT_JSON_PATH.to_string());

    if has_flag(args, "--apply") {
        println!(
            "（--calibrate 模式下 --apply 无意义：校准自己会写回/恢复音量；要指定值请用 --restore）"
        );
    }

    println!("=== 左右音量平衡校准 ===");
    println!(
        "  配置：测试音 {:.0} Hz，{} ms，幅度 {:.2}；单次录音窗口 {} ms（前置 {} + 测试 {} + 尾部 {}）",
        stereo_audio::sync::BALANCE_TONE_HZ,
        cfg.probe_ms,
        cfg.amplitude,
        cfg.window_ms(),
        cfg.pre_roll_ms,
        cfg.probe_ms,
        cfg.tail_ms
    );
    println!(
        "  锚点（{} 个）：{}",
        anchors.len(),
        anchors
            .iter()
            .map(|a| format!("{:.0}%", a * 100.0))
            .collect::<Vec<_>>()
            .join(" / ")
    );
    println!("  曲线扫描点（CURVE_SCALARS）：{CURVE_SCALARS:?}");
    println!(
        "  进度总步数：{}（左/右端点曲线各 1 步 + 每个锚点 1 步，最后恢复音量）",
        2 + anchors.len()
    );
    match restore {
        Some(scalar) => println!("  收尾音量：两端写回 {scalar:.3}（--restore）"),
        None => println!("  收尾音量：恢复校准前的原值（未给 --restore）"),
    }
    println!("  校准结果 JSON：{json_path}（可用 --json <路径> 覆盖）");

    let mic = pick_mic(mic_spec)?;
    print_current("校准前", left);
    print_current("校准前", right);

    let started = Instant::now();
    let outcome = calibrate_balance_with_progress(
        &mic,
        left,
        right,
        cfg,
        &anchors,
        restore,
        |progress: &BalanceProgress| {
            println!(
                "  [{}/{}] {}",
                progress.done, progress.total, progress.message
            );
            let _ = std::io::stdout().flush();
        },
    )?;
    let elapsed = started.elapsed();

    let cal = &outcome.calibration;
    println!("\n--- 校准完成（耗时 {:.1} s）---", elapsed.as_secs_f64());
    println!("  校准数据：{}", cal.describe());
    println!(
        "  版本 v{}（BALANCE_CAL_VERSION = {BALANCE_CAL_VERSION}），左端点 = {}，右端点 = {}",
        cal.version, cal.left_endpoint, cal.right_endpoint
    );

    println!(
        "\n--- 锚点明细（{} 个，按 master 升序）---",
        cal.anchors.len()
    );
    for anchor in &cal.anchors {
        println!("  {}", cal.anchor_label(anchor));
        println!(
            "    master {:.4}（taper {:.2} dB）｜ delta {:+.2} dB ｜ 左 {:.2} / 右 {:.2} dBFS ｜ 置信度 {:.2}",
            anchor.master,
            anchor.master_db,
            anchor.delta_db,
            anchor.left_db,
            anchor.right_db,
            anchor.confidence
        );
    }

    println!("\n--- 端点曲线 ---");
    print_curve("左", &cal.left_curve);
    print_curve("右", &cal.right_curve);
    println!(
        "  sens_diff_db = {:.2} dB（左 − 右，满音量 m = 1.0 处；锚点之间按 dB 线性插值）",
        cal.sens_diff_db
    );

    print_verification(cal);

    println!("\n--- 警告（{} 条）---", outcome.warnings.len());
    if outcome.warnings.is_empty() {
        println!("  无警告");
    } else {
        for (index, warning) in outcome.warnings.iter().enumerate() {
            println!("  {}. {warning}", index + 1);
        }
    }

    match &outcome.last_report {
        Some(report) => println!(
            "\n--- 最后一次锚点的原始测量报告 ---\n  {}",
            report.describe()
        ),
        None => println!("\n  （没有原始测量报告）"),
    }

    println!("\n--- 音量恢复检查 ---");
    print_restore_check("左", left, outcome.left_before.as_ref(), restore);
    print_restore_check("右", right, outcome.right_before.as_ref(), restore);

    println!("\n--- 校准 JSON ---");
    match write_calibration_json(cal, &json_path) {
        Ok(()) => println!(
            "  已写出：{json_path}\n  （config.json 的 balance 字段形状；锚点 {} 个，曲线 左 {} / 右 {} 点）",
            cal.anchors.len(),
            cal.left_curve.points.len(),
            cal.right_curve.points.len()
        ),
        Err(err) => println!(
            "  ⚠ 写出失败：{err}（校准结果仍在上面的输出里；可用 --json <路径> 换位置）"
        ),
    }

    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || has_flag(&args, "--help") || has_flag(&args, "-h") {
        print_usage();
        return Ok(());
    }
    if has_flag(&args, "--list") {
        return list_mode();
    }

    let mic_spec = arg_value(&args, "--mic").ok_or("必须给 --mic <子串>")?;
    let left_spec = arg_value(&args, "--left").ok_or("必须给 --left <端点>")?;
    let right_spec = arg_value(&args, "--right").ok_or("必须给 --right <端点>")?;
    let (left_name, left) = resolve_render(&left_spec)?;
    let (right_name, right) = resolve_render(&right_spec)?;
    println!("左端点：{left_name} [{left}]");
    println!("右端点：{right_name} [{right}]");

    let mut cfg = SyncConfig::default();
    if let Some(v) = arg_value(&args, "--probe-ms") {
        cfg.probe_ms = v
            .parse::<u32>()
            .map_err(|_| format!("--probe-ms 非法：{v}"))?;
    }
    if let Some(v) = arg_value(&args, "--amplitude") {
        cfg.amplitude = v
            .parse::<f32>()
            .map_err(|_| format!("--amplitude 非法：{v}"))?;
    }
    cfg.probe_ms = cfg.probe_ms.clamp(200, 2_000);
    cfg.amplitude = cfg.amplitude.clamp(0.05, 1.0);

    if has_flag(&args, "--calibrate") {
        calibrate_mode(&args, &mic_spec, &left, &right, &cfg)
    } else {
        measure_mode(&args, &mic_spec, &left, &right, &cfg)
    }
}
