# vendor/fusionplay —— 打过低延迟补丁的 FusionPlay MiPlay SDK

## 这是什么

`fusionplay/` 下两个 crate 复制自 FusionPlay-Android 仓库：

| 目录 | 上游路径 | 版本 |
|---|---|---|
| `fusionplay/FusionPlay.MiPlaySdk` | `src/FusionPlay.MiPlaySdk` | 1.0.4 |
| `fusionplay/FusionPlay.AndroidIfAddrs` | `src/FusionPlay.AndroidIfAddrs` | 0.15.0（crate 名 `if-addrs`） |

- 上游：<https://github.com/rosienosiesie/FusionPlay-Android>
- 基线提交：`b43fe1130c3fdb233dcb41c2da174b858eb44cc2`
- 许可：**AGPL-3.0-only**（两个 crate 都继承上游许可）

`receiver/crates/miplay-recv` 用 `path = "../../vendor/fusionplay/FusionPlay.MiPlaySdk"`
依赖它，因此 `miplay-recv.exe` 也是 AGPL-3.0-only；StereoLink 主程序不链接它。

## 为什么要复制一份

上游把「组播播放目标延迟」和「最小排队延迟」**写死**成 800 ms / 650 ms：这是为了和小米
「全屋播放」里其它设备严格对齐，代价是只投本机时也要先攒 0.65~0.8 秒才出声。实测投到
这台电脑上比手机/小米音箱明显慢半拍，而 SDK 没有跳过这段缓冲的开关。

复制出来后把它改成**可用环境变量覆盖**，默认值仍等于上游值：

| 环境变量 | 默认 | 范围 |
|---|---|---|
| `FUSIONPLAY_MIPLAY_GROUP_DELAY_MS` | 800 | 50–1500 |
| `FUSIONPLAY_MIPLAY_MIN_QUEUE_MS` | 650 | 50–1500 |

`miplay-recv --group-delay-ms 220 --min-queue-ms 160` 就是写入这两个变量。

## 改动清单（与上游的唯一差异）

文件：`fusionplay/FusionPlay.MiPlaySdk/src/media.rs`

1. 删掉常量 `MIPLAY_GROUP_PLAY_DELAY_MILLIS` / `MIPLAY_MINIMUM_QUEUE_DELAY_MILLIS`，
   换成 `group_play_delay_millis()` / `minimum_queue_delay_millis()` / `env_delay_millis()`
   三个函数，读上面两个环境变量（`OnceLock` 缓存，`clamp(50, MIPLAY_MAX_BUFFER_MILLIS)`）。
   默认值与上游一致 ⇒ 不设变量时行为与上游完全相同。
2. 两处使用点改为调用函数：
   - `let queue_latency_millis = (group_play_delay_millis() as f64 - output_latency_millis).max(minimum_queue_delay_millis() as f64);`
   - 两处诊断事件里的 `"group_target_delay_ms"` 字段。

此外删掉了上游的 `Cargo.lock`（两个 crate 由本仓库的 `receiver/Cargo.lock` 统一锁版本），
没有其它逻辑改动。所有改动处都带 `StereoLink 补丁` 字样，可用
`Select-String -Path vendor -Pattern "StereoLink 补丁"` 找到。

## 怎么升级上游

1. 覆盖 `fusionplay/` 两个目录为新的上游版本；
2. 按上面的清单重新打补丁（搜索 `MIPLAY_GROUP_PLAY_DELAY_MILLIS` 应无命中）；
3. `cd receiver && cargo build --release` 编译验证。

## 再分发注意

AGPL-3.0 要求分发二进制时提供对应源码。本项目把打补丁后的源码放在本目录里，随
`receiver/` 一起分发即可满足；如果只在本机自用，不涉及分发义务。
