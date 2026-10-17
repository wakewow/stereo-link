//! 诊断：查小米音箱的云端播放状态（`player_get_play_status`），看能否读出播放位置。
//!
//! 用法：`cargo run -p miplay-server --example probe_play_status -- <did|device_id> [次数] [间隔ms]`
//! 每次打印完整 JSON；若同一个流两次读数之间的 `position` 增长 ≈ 实际经过时间，
//! 就能用它算「推流位置 - 播放位置 = 端到端延迟」。

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use miplay_core::{MiToken, MinaClient};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let key = args.next().unwrap_or_else(|| "931892755".to_string());
    let times: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(2);
    let gap_ms: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3000);

    let token = MiToken::load_default().context("加载小米 token 失败")?;
    let client = MinaClient::new(token)?;
    let devices = client.device_list().await?;
    let dev = devices
        .iter()
        .find(|d| d.did == key || d.device_id == key)
        .with_context(|| format!("设备列表里找不到 {key}"))?;
    println!(
        "目标设备: {} (did={} device_id={}) online={}",
        dev.label(),
        dev.did,
        dev.device_id,
        dev.online()
    );

    let mut prev: Option<(Instant, serde_json::Value)> = None;
    for i in 0..times {
        let t = Instant::now();
        let st = client.play_status(&dev.device_id).await?;
        println!("\n--- 第 {} 次 @ {:?} ---", i + 1, t.elapsed());
        println!("{}", serde_json::to_string_pretty(&st)?);
        if let Some((pt, pv)) = prev.take() {
            let dt = pt.elapsed();
            println!("\n距上次读数: {:.2}s", dt.as_secs_f64());
            for field in ["position", "play_time", "current_time", "duration", "elapsed"] {
                let a = find_num(&pv, field);
                let b = find_num(&st, field);
                if let (Some(a), Some(b)) = (a, b) {
                    println!("  {field}: {a} -> {b} (delta {:+}, 墙钟 delta {:.2}s)", b - a, dt.as_secs_f64());
                }
            }
        }
        prev = Some((t, st));
        if i + 1 < times {
            tokio::time::sleep(Duration::from_millis(gap_ms)).await;
        }
    }
    Ok(())
}

/// 在 JSON 里递归找第一个名为 field 的数字字段。
fn find_num(v: &serde_json::Value, field: &str) -> Option<f64> {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(n) = map.get(field).and_then(|x| x.as_f64()) {
                return Some(n);
            }
            map.values().find_map(|x| find_num(x, field))
        }
        serde_json::Value::Array(items) => items.iter().find_map(|x| find_num(x, field)),
        _ => None,
    }
}
