//! 端点音量探针（真机验证用）：列出 / 读取 / 设置 / 还原本机音频端点的音量节点。
//!
//! 用法（在仓库根目录跑，基线文件默认写 `.scratch/volume/baseline.txt`）：
//!
//! ```text
//! volume_probe --list
//! volume_probe --get    <端点id|名字子串|default>
//! volume_probe --set    <sel> <0..1>
//! volume_probe --mute   <sel> <true|false>
//! volume_probe --save   [基线文件]
//! volume_probe --restore [基线文件]
//! ```
//!
//! `--get/--set/--mute` 的选择器：完整端点 id（`{0.0.0.00000000}.{guid}`）、
//! 名字子串（如 `音箱 A`）、或 `default`（默认播放端点）。
//!
//! `--save` 把当前所有活动端点的 (id, 音量标量, 静音) 存成 TSV 基线；
//! `--restore` 按基线逐条写回并读回校验——真机实验「改完必须还原」靠它兜底。

use std::fs;
use std::path::{Path, PathBuf};

use stereo_audio::{get_endpoint_volume, list_endpoints, set_endpoint_mute, set_endpoint_volume};

/// 默认基线文件（相对当前工作目录，即仓库根）。
const DEFAULT_BASELINE: &str = ".scratch/volume/baseline.txt";

type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

fn main() -> AnyResult<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--list") => cmd_list(),
        Some("--get") => cmd_get(arg(&args, 0)?),
        Some("--set") => cmd_set(arg(&args, 0)?, arg(&args, 1)?),
        Some("--mute") => cmd_mute(arg(&args, 0)?, arg(&args, 1)?),
        Some("--save") => cmd_save(args.get(1).map(String::as_str)),
        Some("--restore") => cmd_restore(args.get(1).map(String::as_str)),
        _ => {
            print_usage();
            Ok(())
        }
    }
}

fn print_usage() {
    println!(
        "用法:
  volume_probe --list
  volume_probe --get     <端点id|名字子串|default>
  volume_probe --set     <sel> <0..1>
  volume_probe --mute    <sel> <true|false>
  volume_probe --save    [基线文件]
  volume_probe --restore [基线文件]"
    );
}

/// 取第 `n` 个位置参数（0 = 子命令后面的第一个参数）。
fn arg(args: &[String], n: usize) -> AnyResult<&str> {
    args.get(n + 1)
        .map(String::as_str)
        .ok_or_else(|| format!("缺少第 {} 个参数", n + 1).into())
}

fn cmd_list() -> AnyResult<()> {
    let eps = list_endpoints()?;
    println!("共 {} 个活动端点（render + capture）", eps.len());
    for ep in &eps {
        match get_endpoint_volume(&ep.id) {
            Ok(v) => println!(
                "[{}] {:<36} | {} | {}",
                ep.flow,
                ep.name,
                v.describe(),
                ep.id
            ),
            Err(e) => println!(
                "[{}] {:<36} | <音量读不到：{e}> | {}",
                ep.flow, ep.name, ep.id
            ),
        }
    }
    Ok(())
}

fn cmd_get(sel: &str) -> AnyResult<()> {
    let v = get_endpoint_volume(sel)?;
    println!("{sel} -> {}", v.describe());
    Ok(())
}

fn cmd_set(sel: &str, raw: &str) -> AnyResult<()> {
    let target: f32 = raw
        .parse()
        .map_err(|_| format!("音量必须是数字（0..1），收到 {raw:?}"))?;
    let before = get_endpoint_volume(sel)?;
    println!("before : {}", before.describe());
    match set_endpoint_volume(sel, target) {
        Ok(after) => {
            println!("after  : {}", after.describe());
            Ok(())
        }
        Err(e) => {
            println!("set 失败：{e}");
            if let Ok(after) = get_endpoint_volume(sel) {
                println!("readback: {}", after.describe());
            }
            Err(e.into())
        }
    }
}

fn cmd_mute(sel: &str, raw: &str) -> AnyResult<()> {
    let muted: bool = match raw {
        "true" | "1" | "on" => true,
        "false" | "0" | "off" => false,
        _ => return Err(format!("mute 参数只能是 true/false，收到 {raw:?}").into()),
    };
    let before = get_endpoint_volume(sel)?;
    println!("before : {}", before.describe());
    match set_endpoint_mute(sel, muted) {
        Ok(after) => {
            println!("after  : {}", after.describe());
            Ok(())
        }
        Err(e) => {
            println!("mute 失败：{e}");
            if let Ok(after) = get_endpoint_volume(sel) {
                println!("readback: {}", after.describe());
            }
            Err(e.into())
        }
    }
}

fn baseline_path(arg: Option<&str>) -> PathBuf {
    arg.map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_BASELINE))
}

fn ensure_parent(path: &Path) -> AnyResult<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir)?;
        }
    }
    Ok(())
}

fn cmd_save(arg: Option<&str>) -> AnyResult<()> {
    let path = baseline_path(arg);
    ensure_parent(&path)?;
    let mut out = String::from("# id\tscalar\tmuted\n");
    let mut n = 0usize;
    for ep in list_endpoints()? {
        match get_endpoint_volume(&ep.id) {
            Ok(v) => {
                out.push_str(&format!("{}\t{:.6}\t{}\n", ep.id, v.scalar, v.muted));
                n += 1;
            }
            Err(e) => println!("跳过 {:<36}（音量读不到：{e}）", ep.name),
        }
    }
    fs::write(&path, out)?;
    println!("已保存 {n} 个端点音量到 {}", path.display());
    Ok(())
}

fn cmd_restore(arg: Option<&str>) -> AnyResult<()> {
    let path = baseline_path(arg);
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("读不到基线文件 {}：{e}（先跑 --save）", path.display()))?;
    let mut ok = 0usize;
    let mut fail = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split('\t');
        let (Some(id), Some(scalar), Some(muted)) = (parts.next(), parts.next(), parts.next())
        else {
            println!("忽略格式不对的基线行：{line}");
            fail += 1;
            continue;
        };
        let (Ok(scalar), Ok(muted)) = (scalar.parse::<f32>(), muted.parse::<bool>()) else {
            println!("忽略基线行（数值解析失败）：{line}");
            fail += 1;
            continue;
        };
        match set_endpoint_volume(id, scalar) {
            Ok(v) => {
                let mute_ok = set_endpoint_mute(id, muted);
                let readback = get_endpoint_volume(id);
                let restored = match (&mute_ok, &readback) {
                    (Ok(_), Ok(rb)) => (rb.scalar - scalar).abs() <= 0.02 && rb.muted == muted,
                    _ => false,
                };
                if restored {
                    ok += 1;
                    println!("还原 {id} -> {}", v.describe());
                } else {
                    fail += 1;
                    println!(
                        "还原 {id} 后校验失败：target scalar={scalar:.3} muted={muted}；\
                         get={:?} mute={:?}",
                        readback.map(|r| r.scalar),
                        mute_ok.map(|m| m.muted)
                    );
                }
            }
            Err(e) => {
                fail += 1;
                println!("还原 {id} 失败：{e}");
            }
        }
    }
    println!(
        "还原完成：{ok} 个成功，{fail} 个失败（基线 {}）",
        path.display()
    );
    if fail > 0 {
        return Err(format!("{fail} 个端点没有还原成功").into());
    }
    Ok(())
}
