# 小米云 API 参考（来自 `miservice` 3.0.1 源码）

> 来源：`miservice` 包源码，MIT，作者 Yonsm — <https://github.com/Yonsm/MiService>
> 本机路径：`C:\Users\Administrator\AppData\Roaming\uv\tools\mixplay-hub\Lib\site-packages\miservice\`
>
> 本文是**逐行核对源码**后的摘要，供 Rust 移植使用。行号对应 `miservice/minaservice.py`。

## 1. 鉴权（`miaccount.py`）

- 服务 id（sid）：小爱音箱相关的所有调用都用 **`micoapi`**。
- 请求带上两个 Cookie：
  - `userId=<userId>`
  - `serviceToken=<token["micoapi"][1]>`
- `token` 文件结构（MiService / MiXPlay 兼容）：

  ```json
  {
    "deviceId": "<16 位>",
    "userId": "<10 位>",
    "passToken": "V1:…",
    "micoapi": ["<ssecurity>", "<serviceToken>"]
  }
  ```

  其中 `micoapi[0]` = `ssecurity`，`micoapi[1]` = `serviceToken`。
- 鉴权失效判定：HTTP 401，**或** HTTP 200 但 body 是 JSON 且 `code != 0` 且
  `message` 里包含 `"auth"`（`xiaomi.py:166`）。失效后清掉该 sid 的 token 并**重试一次**。
- 换票流程（`xiaomi.py:102-143`）：`serviceLogin?sid=micoapi&_json=true` →
  若 `code != 0` 则用 `serviceLoginAuth2`（`hash = MD5(密码).hexdigest().upper()`）→
  可能触发 `notificationUrl` 二次验证 → 拿 `userId/passToken/location/nonce/ssecurity` →
  `_securityTokenService()` 换取 `serviceToken`。

## 2. 请求封装（`minaservice.py:30-41`）

```
POST/GET https://api2.mina.mi.com<uri>
Headers: User-Agent = UA_MINA
Cookies: userId, serviceToken
```

- 每个请求都要带 **`requestId = "app_ios_" + 30 位随机串`**：
  - GET 时拼在 query：`uri += "&requestId=" + requestId`
  - POST 时放进 body：`data["requestId"] = requestId`
- `UA_MINA`：`MiHome/6.0.103 (com.xiaomi.mihome; build:6.0.103.1; iOS 14.4.0) Alamofire/6.0.103 MICO/iOSApp/appStore/6.0.103`

## 3. 设备列表（`minaservice.py:39-41`）

| 方法 | 路径 | 返回 |
| --- | --- | --- |
| GET | `/admin/v2/device_list?master=0` | `resp["data"]` = 设备数组 |

`master=0` 表示列出账号下**全部**设备（=1 只列主设备）。

数组中每个元素的关键字段：`deviceID`（MiNA uuid）、`miotDID`、`name`、`hardware`、
`model`、`mac`、`presence` / `isOnline`、`capabilities`（如 `yunduantts`、`stereo_mode_v2`）。

## 4. ubus 调用（`minaservice.py:43-51`）★ 核心

```
POST https://api2.mina.mi.com/remote/ubus
body: {
  "deviceId": "<MiNA uuid>",
  "method":   "<ubus method>",
  "path":     "<ubus object>",
  "message":  "<JSON 字符串>",
  "requestId":"app_ios_…"
}
```

成功判定：`resp["code"] == 0`。

### 4.1 播放控制

| 用途 | method | path | message |
| --- | --- | --- | --- |
| 播放 URL（通用机型） | `player_play_url` | `mediaplayer` | `{"url": url, "type": 2, "media": "app_ios"}` |
| 播放 URL（特定机型） | `player_play_music` | `mediaplayer` | `{"startaudioid": audio_id, "music": "<JSON 字符串>"}` |
| 暂停 | `player_play_operation` | `mediaplayer` | `{"action": "pause", "media": "app_ios"}` |
| 停止 | `player_play_operation` | `mediaplayer` | `{"action": "stop", "media": "app_ios"}` |
| 继续 | `player_play_operation` | `mediaplayer` | `{"action": "play", "media": "app_ios"}` |
| 设置音量 | `player_set_volume` | `mediaplayer` | `{"volume": <int>, "media": "app_ios"}` |
| 查询状态 | `player_get_play_status` | `mediaplayer` | `{"media": "app_ios"}` |
| 循环模式 | `player_set_loop` | `mediaplayer` | `{"media": "common", "type": <0\|1\|2>}` |
| TTS 说话 | `text_to_speech` | `mibrain` | `{"text": "<文本>"}` |

`player_get_play_status` 的真实负载在 `resp["data"]["info"]`，是一个 **JSON 字符串**，需要二次解析。

### 4.2 `player_play_music` 的完整报文（`minaservice.py:106-142`）

必须使用 `player_play_music` 的机型白名单（`minaservice.py:11-15`）：

```
LX04 LX05 L05B L05C L06 L06A X08A X10A X08C X08E X8F X4B OH2 OH2P X6A
```

> **注意**：`L16A` / `L16B`（Xiaomi Sound 及 Xiaomi Sound-左）**不在**白名单里 →
> 按 MiService 的逻辑它们应该走 `player_play_url`，比 `player_play_music` 更简单。
> 值得实测对比两者的延迟。

```jsonc
{
  "startaudioid": "1582971365183456177",
  "music": "<下面这个对象的 JSON 字符串>"
}
```

```jsonc
{
  "payload": {
    "audio_type": "",              // type==1 时填 "MUSIC"（会点亮部分音箱的灯环）
    "audio_items": [
      {
        "item_id": {
          "audio_id": "1582971365183456177",
          "cp": { "album_id": "-1", "episode_index": 0,
                  "id": "355454500", "name": "xiaowei" }
        },
        "stream": { "url": "<音频 URL>" }
      }
    ],
    "list_params": {
      "listId": "-1", "loadmore_offset": 0,
      "origin": "xiaowei", "type": "MUSIC"
    }
  },
  "play_behavior": "REPLACE_ALL"
}
```

默认值：`audio_id = "1582971365183456177"`，`cp_id = "355454500"`。

## 5. 与本项目已知事实的对照

- 本机账号下 3 台设备：`Xiaomi Sound`(L16B, did 934947888)、`Xiaomi Sound-左`(L16A, did 501718792)、
  `小爱音箱Play增强版`(L05C, did 931892755)。
- 立体声组合 ＝ L16A + L16B 在妙播里成对；小米云 `device_list` 里**没有** group 对象，
  只有 `stereo_mode_v2` 能力位 → 需要**分别**给两只下指令。
- 已验证可用的高延迟链路：本机起 HTTP WAV 流 → 云 `player_play_music`/`player_play_url`
  下发 → 音箱自己回拉 → 出声（体感 1~2 秒）。
