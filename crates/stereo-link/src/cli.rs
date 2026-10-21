//! 命令行入口（`stereo-link-cli.exe`）：状态、日志、动作、配置、部署全在这里。
//!
//! 约定（见 `docs/dev-conventions.md`）：
//! * 每个界面动作都有同名命令，脚本/CI 不用点按钮；
//! * 默认输出人读文本，`--json` 输出**恰好一个** JSON 文档（方便 `ConvertFrom-Json` / `jq`）；
//! * 退出码：0 成功、1 失败、2 用法错误。
//!
//! 能独立工作（不需要实例在跑）的命令：`doctor`、`config *`、`endpoints`、`log`（读落盘文件）、
//! `serve`。需要实例的命令会通过 `control.json` 连本机 GUI / serve。

use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::AppConfig;
use crate::control;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 程序入口（`src/bin/stereo-link-cli.rs` 只调用它）。
pub fn main() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match run(&argv) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("错误：{message}");
            1
        }
    }
}

/// 解析出来的全局开关。
#[derive(Debug, Clone, Copy, Default)]
struct Globals {
    json: bool,
    verbose: bool,
}

fn run(argv: &[String]) -> Result<i32, String> {
    let mut globals = Globals::default();
    let mut words: Vec<&str> = Vec::new();
    for word in argv {
        match word.as_str() {
            "--json" | "-j" => globals.json = true,
            "-v" | "--verbose" => globals.verbose = true,
            "-h" | "--help" | "help" => {
                println!("{HELP}");
                return Ok(0);
            }
            "-V" | "--version" | "version" => {
                if globals.json {
                    println!(
                        "{}",
                        json!({ "version": VERSION, "protocol": control::PROTOCOL_VERSION })
                    );
                } else {
                    println!("stereo-link-cli {VERSION}");
                }
                return Ok(0);
            }
            other => words.push(other),
        }
    }
    if globals.verbose {
        std::env::set_var("STEREOLINK_LOG_CONSOLE", "1");
    }
    let Some(command) = words.first().copied() else {
        println!("{HELP}");
        return Ok(2);
    };
    let rest = &words[1..];
    dispatch(globals, command, rest)
}

fn dispatch(globals: Globals, command: &str, rest: &[&str]) -> Result<i32, String> {
    match command {
        "ping" => call(globals, json!({ "cmd": "ping" })),
        "status" => status(globals),
        "log" => log(globals, rest),
        "routing" => routing(globals, rest),
        "receiver" | "receivers" | "cast" => receiver(globals, rest),
        "endpoints" | "devices" => endpoints(globals, rest),
        "config" | "cfg" => config(globals, rest),
        "tone" => tone(globals, rest),
        "doctor" => doctor(globals),
        "serve" => serve(globals, rest),
        "quit" | "stop" => call(globals, json!({ "cmd": "quit" })),
        other => Err(format!("未知命令：{other}（用 --help 看用法）")),
    }
}

// ------------------------------------------------------------------ 子命令

fn status(globals: Globals) -> Result<i32, String> {
    match control::request(json!({ "cmd": "status" })) {
        Ok(data) => {
            if globals.json {
                print_json(&data);
            } else {
                print_status(&data);
            }
            Ok(0)
        }
        Err(error) => {
            // 没实例在跑也算一次有效查询：给离线概览，退出码仍是 0；
            // 想严格判断「在不在跑」用 `ping`/`doctor`。
            if globals.json {
                print_json(&json!({ "running": false, "reason": error }));
            } else {
                println!("没有运行中的 StereoLink 实例（{error}）");
                println!();
                println!("离线概览：");
                println!("  配置：{}", AppConfig::path().display());
                println!("  日志：{}", control::log_path().display());
                if let Ok(endpoints) = stereo_audio::list_endpoints() {
                    let renders: Vec<_> = endpoints
                        .iter()
                        .filter(|ep| ep.flow == stereo_audio::DataFlow::Render)
                        .collect();
                    let default = renders.iter().find(|ep| ep.is_default);
                    println!(
                        "  播放端点：{} 个，系统默认 {}",
                        renders.len(),
                        default.map(|ep| ep.name.as_str()).unwrap_or("<读不到>")
                    );
                }
                println!();
                println!("提示：GUI 直接运行 stereo-link.exe；无头部署用 stereo-link-cli serve");
            }
            Ok(0)
        }
    }
}

fn print_status(data: &Value) {
    let mode = text(data, "mode");
    let version = text(data, "version");
    let pid = data.get("pid").and_then(Value::as_u64).unwrap_or(0);
    let uptime = data
        .get("uptime_seconds")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    println!("StereoLink {version}（{mode}，pid {pid}，已运行 {uptime:.0}s）");

    let routing = &data["routing"];
    let running = routing
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if running {
        let engine = &routing["engine"];
        let source = text(engine, "source_device");
        let rate = engine.get("rate").and_then(Value::as_u64).unwrap_or(0);
        println!("路由：运行中（{source}，{rate} Hz）");
        if let Some(routes) = engine.get("routes").and_then(Value::as_array) {
            for route in routes {
                println!(
                    "  {} → {}（排队 {} ms，欠载 {}，丢帧 {} ms）",
                    text(route, "channel"),
                    text(route, "endpoint"),
                    number(route, "queued_ms"),
                    number(route, "underruns"),
                    number(route, "dropped_ms"),
                );
            }
        }
    } else {
        println!("路由：未运行");
    }

    let receivers = &data["receivers"];
    let mut line = String::from("投放：");
    if let Some(items) = receivers.get("items").and_then(Value::as_array) {
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                line.push('，');
            }
            let label = text(item, "label");
            let state = if item.get("running").and_then(Value::as_bool) == Some(true) {
                "运行中"
            } else {
                "未运行"
            };
            line.push_str(&format!("{label} {state}"));
            if let Some(error) = item.get("error").and_then(Value::as_str) {
                line.push_str(&format!("（{error}）"));
            }
        }
    }
    if let Some(device) = receivers.get("device").and_then(Value::as_str) {
        line.push_str(&format!("；输出声卡 {device}"));
    }
    println!("{line}");

    if let Some(default) = data["extra"]["default_output"]
        .get("name")
        .and_then(Value::as_str)
    {
        println!("默认输出：{default}");
    }
    if let Some(path) = data["config"].get("path").and_then(Value::as_str) {
        println!("配置：{path}");
    }
    if let Some(path) = data.pointer("/extra/log_path").and_then(Value::as_str) {
        println!("日志：{path}");
    }
}

fn log(globals: Globals, rest: &[&str]) -> Result<i32, String> {
    let mut lines = 200usize;
    let mut level: Option<String> = None;
    let mut follow = false;
    let mut from_file = false;
    let mut index = 0;
    while index < rest.len() {
        match rest[index] {
            "-n" | "--lines" => {
                index += 1;
                lines = rest
                    .get(index)
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| "-n 需要一个行数".to_owned())?;
            }
            "--level" => {
                index += 1;
                level = rest.get(index).map(|value| (*value).to_owned());
            }
            "-f" | "--follow" => follow = true,
            "--file" => from_file = true,
            other => return Err(format!("log 不认识参数：{other}")),
        }
        index += 1;
    }

    if from_file || control::running().is_none() {
        return log_from_file(globals, lines, level.as_deref(), follow);
    }
    let mut request = json!({ "cmd": "log", "lines": lines });
    if let Some(level) = &level {
        request["level"] = json!(level);
    }
    let data = control::request(request)?;
    if globals.json {
        print_json(&data);
    } else {
        for line in data
            .get("lines")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            println!(
                "[{:>7.1}s] {:<5} {}",
                number(line, "at_ms") as f64 / 1000.0,
                text(line, "level"),
                text(line, "text")
            );
        }
    }
    if follow {
        println!("（--follow 只支持 --file 模式；实例在跑时请反复执行本命令，或直接用 --file -f）");
    }
    Ok(0)
}

fn log_from_file(
    globals: Globals,
    lines: usize,
    level: Option<&str>,
    follow: bool,
) -> Result<i32, String> {
    let path = control::log_path();
    if !path.exists() {
        return Err(format!("日志文件还不存在：{}", path.display()));
    }
    let filtered = |text: &str| match level {
        Some(level) => text
            .to_ascii_uppercase()
            .contains(&level.to_ascii_uppercase()),
        None => true,
    };
    let content = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let kept: Vec<&str> = content
        .lines()
        .filter(|line| !line.is_empty() && filtered(line))
        .collect();
    if globals.json {
        print_json(&json!({ "path": path.display().to_string(), "lines": kept }));
    } else {
        for line in kept.iter().skip(kept.len().saturating_sub(lines)) {
            println!("{line}");
        }
    }
    if follow {
        follow_file(&path, filtered, globals.json)?;
    }
    Ok(0)
}

fn follow_file(
    path: &PathBuf,
    filtered: impl Fn(&str) -> bool,
    json_out: bool,
) -> Result<(), String> {
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut offset = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(300));
                // 轮转或截断：回到新文件末尾继续跟。
                if let Ok(meta) = std::fs::metadata(path) {
                    if meta.len() < offset {
                        offset = 0;
                        if let Ok(new_file) = std::fs::File::open(path) {
                            reader = BufReader::new(new_file);
                        }
                    }
                }
            }
            Ok(read) => {
                offset += read as u64;
                let text = line.trim_end();
                if !text.is_empty() && filtered(text) {
                    if json_out {
                        print_json(&json!({ "line": text }));
                    } else {
                        println!("{text}");
                    }
                }
            }
            Err(error) => return Err(format!("读日志失败：{error}")),
        }
    }
}

fn routing(globals: Globals, rest: &[&str]) -> Result<i32, String> {
    let action = rest.first().copied().unwrap_or("status");
    let data = control::request(json!({ "cmd": "routing", "action": action }))?;
    if globals.json {
        print_json(&data);
    } else {
        match action {
            "status" => {
                let running = data
                    .get("running")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                println!("路由：{}", if running { "运行中" } else { "未运行" });
            }
            "stop" => println!("路由已停止"),
            "start" => println!("路由已启动"),
            "restart" => println!("路由已重启"),
            "mute" => println!("路由已静音"),
            "unmute" => println!("路由已取消静音"),
            other => println!("路由：{other} 完成"),
        }
    }
    Ok(0)
}

fn receiver(globals: Globals, rest: &[&str]) -> Result<i32, String> {
    let action = rest.first().copied().unwrap_or("list");
    let kind = rest.get(1).copied().unwrap_or("all");
    let data = control::request(json!({ "cmd": "receiver", "action": action, "kind": kind }))?;
    if globals.json {
        print_json(&data);
        return Ok(0);
    }
    if let Some(items) = data.get("items").and_then(Value::as_array) {
        for item in items {
            let running = item
                .get("running")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let state = if running { "运行中" } else { "未运行" };
            let status = text(item, "status");
            // 状态文字和「运行中/未运行」重复时就不用打两遍。
            let status = if status == state { "" } else { status };
            println!(
                "{:<8} {:<6} {}{}",
                text(item, "kind"),
                state,
                status,
                item.get("error")
                    .and_then(Value::as_str)
                    .map(|error| format!("（{error}）"))
                    .unwrap_or_default()
            );
        }
        if let Some(device) = data.get("device").and_then(Value::as_str) {
            println!("输出声卡：{device}");
        }
    }
    Ok(0)
}

fn endpoints(globals: Globals, rest: &[&str]) -> Result<i32, String> {
    let flow = rest.first().copied().unwrap_or("render");
    if control::running().is_some() {
        let data = control::request(json!({ "cmd": "endpoints" }))?;
        if globals.json {
            print_json(&data);
            return Ok(0);
        }
        print_endpoints(&data, flow);
        return Ok(0);
    }
    let endpoints = stereo_audio::list_endpoints().map_err(|error| error.to_string())?;
    let data = control::endpoints_json(&endpoints);
    if globals.json {
        print_json(&data);
    } else {
        print_endpoints(&data, flow);
    }
    Ok(0)
}

fn print_endpoints(data: &Value, flow: &str) {
    let want_render = flow != "capture";
    let mut index = 0;
    for item in data
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let item_flow = text(item, "flow");
        if (item_flow == "render") != want_render {
            continue;
        }
        index += 1;
        let name = text(item, "name");
        let id = text(item, "id");
        let default = if item.get("is_default").and_then(Value::as_bool) == Some(true) {
            "  ← 系统默认"
        } else {
            ""
        };
        println!("{index:>3}. {name}{default}");
        println!("     id={id}");
    }
    if index == 0 {
        println!("（没有 {flow} 端点）");
    }
}

fn config(globals: Globals, rest: &[&str]) -> Result<i32, String> {
    let action = rest.first().copied().unwrap_or("show");
    // 实例在跑时走控制面（能改到内存里的那份配置）；否则直接读写配置文件。
    let live = control::running().is_some();
    match action {
        "path" => {
            let path = AppConfig::path();
            if globals.json {
                print_json(&json!({
                    "path": path.display().to_string(),
                    "log_path": control::log_path().display().to_string(),
                    "control_path": control::control_path().display().to_string(),
                }));
            } else {
                println!("配置：{}", path.display());
                println!("日志：{}", control::log_path().display());
                println!("控制面：{}", control::control_path().display());
            }
            Ok(0)
        }
        "show" => {
            if live {
                let data = control::request(json!({ "cmd": "config", "action": "show" }))?;
                if globals.json {
                    print_json(&data);
                } else {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&data).unwrap_or_default()
                    );
                }
                return Ok(0);
            }
            let cfg = AppConfig::load();
            let data = serde_json::to_value(&cfg).unwrap_or(Value::Null);
            if globals.json {
                print_json(&data);
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&data).unwrap_or_default()
                );
            }
            Ok(0)
        }
        "get" => {
            let key = rest.get(1).copied().unwrap_or("");
            let value = if live {
                control::request(json!({ "cmd": "config", "action": "get", "key": key }))?
            } else {
                let cfg = AppConfig::load();
                control::config_get(&cfg, key)?
            };
            if globals.json {
                print_json(&value);
            } else if let Some(inner) = value.get("value") {
                // 文本模式：字符串值直接打内容（`"扬声器"` 打出来带引号很别扭）。
                match inner {
                    Value::String(text) => println!("{text}"),
                    other => {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(other).unwrap_or_default()
                        )
                    }
                }
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value).unwrap_or_default()
                );
            }
            Ok(0)
        }
        "set" => {
            let key = rest
                .get(1)
                .copied()
                .ok_or_else(|| "用法：config set <key> <value>".to_owned())?;
            let raw = rest
                .get(2)
                .copied()
                .ok_or_else(|| format!("用法：config set {key} <value>（字符串值不用加引号）"))?;
            let value = parse_cli_value(raw);
            if live {
                let data = control::request(
                    json!({ "cmd": "config", "action": "set", "key": key, "value": value }),
                )?;
                if globals.json {
                    print_json(&data);
                } else {
                    println!("已设置 {key} = {value}（实例已保存）");
                }
                return Ok(0);
            }
            let mut cfg = AppConfig::load();
            control::config_set(&mut cfg, key, value.clone())?;
            cfg.save();
            if globals.json {
                print_json(&json!({ "key": key, "value": value, "saved": true }));
            } else {
                println!("已设置 {key} = {value}（直接写入配置文件；实例没在跑）");
            }
            Ok(0)
        }
        other => Err(format!("config 不支持 action={other}")),
    }
}

fn tone(globals: Globals, rest: &[&str]) -> Result<i32, String> {
    let mut freq = 440.0f64;
    let mut seconds = 1.0f64;
    let mut device: Option<String> = None;
    let mut index = 0;
    while index < rest.len() {
        match rest[index] {
            "--freq" => {
                index += 1;
                freq = rest
                    .get(index)
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| "--freq 需要一个数字".to_owned())?;
            }
            "--seconds" => {
                index += 1;
                seconds = rest
                    .get(index)
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| "--seconds 需要一个数字".to_owned())?;
            }
            "--device" => {
                index += 1;
                device = rest.get(index).map(|value| (*value).to_owned());
            }
            other => return Err(format!("tone 不认识参数：{other}")),
        }
        index += 1;
    }
    let mut request = json!({ "cmd": "tone", "freq": freq, "seconds": seconds });
    if let Some(device) = &device {
        request["device"] = json!(device);
    }
    match control::request(request.clone()) {
        Ok(data) => {
            if globals.json {
                print_json(&data);
            } else {
                println!(
                    "已播放 {:.0} Hz / {:.1}s 测试音 → {}",
                    freq,
                    seconds,
                    text(&data, "device")
                );
            }
            Ok(0)
        }
        Err(error) => {
            // 没实例时直接在默认设备上播（单机自检用）。
            if !error.contains("control.json") && !error.contains("连不上") {
                return Err(error);
            }
            let selector = match device.as_deref() {
                Some(device) if !device.is_empty() && device != "default" => {
                    stereo_audio::DeviceSelector::name_contains(device)
                }
                _ => stereo_audio::DeviceSelector::Default,
            };
            let info = stereo_audio::play_sine(&selector, freq, seconds, 0.25)
                .map_err(|error| format!("播放测试音失败：{error}"))?;
            if globals.json {
                print_json(&json!({ "played": true, "device": info.device_name }));
            } else {
                println!(
                    "已播放 {freq:.0} Hz / {seconds:.1}s 测试音 → {}",
                    info.device_name
                );
            }
            Ok(0)
        }
    }
}

fn doctor(globals: Globals) -> Result<i32, String> {
    let data = crate::serve::doctor_json();
    if globals.json {
        print_json(&data);
    } else {
        println!("StereoLink 自检 {VERSION}");
        println!("  程序：{}", text(&data, "exe"));
        for check in data
            .get("checks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let ok = check.get("ok").and_then(Value::as_bool).unwrap_or(false);
            println!(
                "  [{}] {}：{}",
                if ok { "✓" } else { "×" },
                text(check, "name"),
                text(check, "detail")
            );
        }
    }
    let ok = data.get("ok").and_then(Value::as_bool).unwrap_or(false);
    Ok(if ok { 0 } else { 1 })
}

fn serve(globals: Globals, rest: &[&str]) -> Result<i32, String> {
    let mut options = crate::serve::Options {
        routing: true,
        receivers: true,
    };
    for word in rest {
        match *word {
            "--no-routing" => options.routing = false,
            "--no-receivers" => options.receivers = false,
            other => return Err(format!("serve 不认识参数：{other}")),
        }
    }
    if globals.json {
        // 长驻进程没法先打 JSON 再干活：退化为一行「事件」JSON，方便日志抓取。
        println!(
            "{}",
            json!({ "event": "serve_starting", "version": VERSION })
        );
    }
    crate::serve::run(options).map_err(|error| format!("{error:#}"))?;
    Ok(0)
}

fn call(globals: Globals, request: Value) -> Result<i32, String> {
    let data = control::request(request)?;
    if globals.json {
        print_json(&data);
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&data).unwrap_or_default()
        );
    }
    Ok(0)
}

// ------------------------------------------------------------------ 小工具

fn print_json(data: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string())
    );
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn number(value: &Value, key: &str) -> i64 {
    value
        .get(key)
        .and_then(Value::as_i64)
        .or_else(|| {
            value
                .get(key)
                .and_then(Value::as_f64)
                .map(|number| number as i64)
        })
        .unwrap_or(0)
}

/// 命令行里的值：先当 JSON 解析（`true` / `42` / `[1,2]`），失败就当字符串。
fn parse_cli_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| json!(raw))
}

/// `--help`。
const HELP: &str = "\
StereoLink 立体声链 —— 命令行

用法：
  stereo-link-cli <命令> [参数] [--json] [-v]

全局开关：
  --json, -j        输出一个 JSON 文档（脚本用）；text 模式默认人读
  -v, --verbose     把日志同时打到控制台
  -V, --version     版本
  -h, --help        本帮助

命令：
  status                引擎 / 接收端 / 默认声卡 / 配置路径一览（实例没跑时给离线概览）
  log [-n 行数] [--level WARN] [-f] [--file]
                        实例在跑读内存日志，否则读落盘日志；-f 只跟文件
  routing [start|stop|restart|mute|unmute|status]
  receiver [list|start|stop|restart] [airplay|miplay|all]
  endpoints [render|capture]
  config [show|get <key>|set <key> <value>|path]
  tone [--freq 440] [--seconds 1.0] [--device 名字片段]
  doctor                环境自检（配置、日志、端点、虚拟声卡、接收端 exe、运行实例）
  serve [--no-routing] [--no-receivers]
                        无头运行：引擎 + 接收端 + 控制面，Ctrl+C 或 quit 退出
  quit                  让运行中的实例退出
  ping                  探活

配置文件：%APPDATA%\\stereo-link\\config.json
日志文件：%APPDATA%\\stereo-link\\logs\\stereo-link.log
控制文件：%APPDATA%\\stereo-link\\control.json
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_values_parse_like_json() {
        assert_eq!(parse_cli_value("true"), json!(true));
        assert_eq!(parse_cli_value("42"), json!(42));
        assert_eq!(parse_cli_value("立体声链"), json!("立体声链"));
    }

    #[test]
    fn help_mentions_every_command() {
        for command in [
            "status",
            "log",
            "routing",
            "receiver",
            "endpoints",
            "config",
            "tone",
            "doctor",
            "serve",
            "quit",
        ] {
            assert!(HELP.contains(command), "帮助里少了 {command}");
        }
    }
}
