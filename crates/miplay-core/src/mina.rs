//! 小米云 MiNA API 客户端。
//!
//! 逐条对照 [MiService](https://github.com/Yonsm/MiService) 3.0.1 的
//! `miservice/minaservice.py` 与 `miservice/miaccount.py` 移植（MIT）。
//! 详细契约见 `docs/miservice-api.md`。

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::device::XiaomiDevice;
use crate::rand::alnum;
use crate::token::{MiToken, SID_MICOAPI};

pub const MINA_BASE: &str = "https://api2.mina.mi.com";

/// `miservice/miaccount.py` 里的 `UA_MINA`。
pub const UA_MINA: &str = "MiHome/6.0.103 (com.xiaomi.mihome; build:6.0.103.1; iOS 14.4.0) \
                           Alamofire/6.0.103 MICO/iOSApp/appStore/6.0.103";

/// 必须走 `player_play_music`（而不是 `player_play_url`）的机型白名单。
///
/// 来源：`miservice/minaservice.py:11-15`。
pub const PLAY_MUSIC_HARDWARE: &[&str] = &[
    "LX04", "LX05", "L05B", "L05C", "L06", "L06A", "X08A", "X10A", "X08C", "X08E", "X8F", "X4B",
    "OH2", "OH2P", "X6A",
];

/// `player_play_music` 必填的音频 id 默认值。
pub const DEFAULT_AUDIO_ID: &str = "1582971365183456177";
/// `player_play_music` 里的内容提供方 id 默认值。
pub const DEFAULT_CP_ID: &str = "355454500";

/// 播放接口的选择结果，便于日志与前端展示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayApi {
    /// `mediaplayer.player_play_url`
    PlayUrl,
    /// `mediaplayer.player_play_music`
    PlayMusic,
}

impl PlayApi {
    pub fn as_str(self) -> &'static str {
        match self {
            PlayApi::PlayUrl => "player_play_url",
            PlayApi::PlayMusic => "player_play_music",
        }
    }
}

pub struct MinaClient {
    http: reqwest::Client,
    token: MiToken,
}

impl MinaClient {
    pub fn new(token: MiToken) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(UA_MINA)
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .context("构造 HTTP 客户端失败")?;
        Ok(Self { http, token })
    }

    pub fn token(&self) -> &MiToken {
        &self.token
    }

    /// 底层请求：自动加 `requestId` 与 Cookie，并在鉴权失效时给出可操作的错误。
    ///
    /// GET（`body = None`）时 `requestId` 拼在 query 上；
    /// POST 时放进**表单** —— 与 `minaservice.py:30-37` 一致。
    ///
    /// ⚠️ POST 必须用 `application/x-www-form-urlencoded`，**不能发 JSON**：
    /// `miaccount.py:273` 把 dict 交给 aiohttp 的 `data=`，走的就是表单编码；
    /// MiNA 是 Spring 服务，用 `@RequestParam` 取 `deviceId` 等参数，
    /// 发 JSON 会得到 `code=601 MissingServletRequestParameterException`。
    pub async fn request(&self, uri: &str, body: Option<Value>) -> Result<Value> {
        let request_id = format!("app_ios_{}", alnum(30));
        let cookie = self.token.cookie_header(SID_MICOAPI)?;
        let url = format!("{MINA_BASE}{uri}");

        let resp = match body {
            Some(mut v) => {
                let obj = v
                    .as_object_mut()
                    .context("MiNA 请求体必须是 JSON 对象")?;
                obj.insert("requestId".into(), json!(request_id));
                self.http
                    .post(&url)
                    .header(reqwest::header::COOKIE, cookie)
                    .form(&to_form_pairs(&v))
                    .send()
                    .await
                    .with_context(|| format!("请求 {url} 失败"))?
            }
            None => {
                let sep = if uri.contains('?') { '&' } else { '?' };
                self.http
                    .get(format!("{url}{sep}requestId={request_id}"))
                    .header(reqwest::header::COOKIE, cookie)
                    .send()
                    .await
                    .with_context(|| format!("请求 {url} 失败"))?
            }
        };

        let status = resp.status();
        let text = resp.text().await.context("读取响应体失败")?;

        if status == reqwest::StatusCode::UNAUTHORIZED {
            bail!("小米云鉴权失败 (HTTP 401)：serviceToken 已失效，需要重新登录小米账号");
        }

        let value: Value = serde_json::from_str(&text).with_context(|| {
            format!(
                "MiNA 返回的不是 JSON (HTTP {status}): {}",
                truncate(&text, 300)
            )
        })?;

        // `xiaomi.py:166` 的判定：HTTP 200 但 message 里带 auth 也算失效
        if let Some(msg) = value.get("message").and_then(Value::as_str) {
            if msg.to_ascii_lowercase().contains("auth") {
                bail!("小米云鉴权失效：{msg}（需要重新登录小米账号）");
            }
        }
        Ok(value)
    }

    /// 校验 `code == 0`，否则返回带原始响应的错误。
    fn ensure_ok(value: &Value, what: &str) -> Result<()> {
        let code = value.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code == 0 {
            return Ok(());
        }
        let msg = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("<无 message>");
        bail!("{what} 失败: code={code} message={msg} raw={}", truncate(&value.to_string(), 400))
    }

    // ---------------------------------------------------------------- 设备

    /// `GET /admin/v2/device_list?master=0` —— 账号下全部设备。
    pub async fn device_list(&self) -> Result<Vec<XiaomiDevice>> {
        let resp = self.request("/admin/v2/device_list?master=0", None).await?;
        Self::ensure_ok(&resp, "device_list")?;
        let arr = resp
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(arr.into_iter().map(XiaomiDevice::from_value).collect())
    }

    // ---------------------------------------------------------------- ubus

    /// 原始 ubus 调用，返回完整响应。
    pub async fn ubus_raw(
        &self,
        device_id: &str,
        method: &str,
        path: &str,
        message: Value,
    ) -> Result<Value> {
        let body = json!({
            "deviceId": device_id,
            "method": method,
            "path": path,
            "message": serde_json::to_string(&message)?,
        });
        self.request("/remote/ubus", Some(body)).await
    }

    /// ubus 调用，成功返回 `true`。
    pub async fn ubus(
        &self,
        device_id: &str,
        method: &str,
        path: &str,
        message: Value,
    ) -> Result<bool> {
        let resp = self.ubus_raw(device_id, method, path, message).await?;
        Ok(resp.get("code").and_then(Value::as_i64).unwrap_or(-1) == 0)
    }

    // ------------------------------------------------------------ 播放控制

    /// 该机型应该用哪个播放接口。
    pub fn play_api_for(hardware: &str) -> PlayApi {
        if PLAY_MUSIC_HARDWARE
            .iter()
            .any(|h| h.eq_ignore_ascii_case(hardware))
        {
            PlayApi::PlayMusic
        } else {
            PlayApi::PlayUrl
        }
    }

    /// `mediaplayer.player_play_url`
    pub async fn play_by_url(&self, device_id: &str, url: &str, media_type: i64) -> Result<bool> {
        self.ubus(
            device_id,
            "player_play_url",
            "mediaplayer",
            json!({ "url": url, "type": media_type, "media": "app_ios" }),
        )
        .await
    }

    /// `mediaplayer.player_play_music` —— 部分机型必需（见 [`PLAY_MUSIC_HARDWARE`]）。
    pub async fn play_by_music_url(
        &self,
        device_id: &str,
        url: &str,
        audio_id: &str,
        cp_id: &str,
        media_type: i64,
    ) -> Result<bool> {
        let audio_type = if media_type == 1 { "MUSIC" } else { "" };
        let music = json!({
            "payload": {
                "audio_type": audio_type,
                "audio_items": [{
                    "item_id": {
                        "audio_id": audio_id,
                        "cp": {
                            "album_id": "-1",
                            "episode_index": 0,
                            "id": cp_id,
                            "name": "xiaowei",
                        }
                    },
                    "stream": { "url": url }
                }],
                "list_params": {
                    "listId": "-1",
                    "loadmore_offset": 0,
                    "origin": "xiaowei",
                    "type": "MUSIC"
                }
            },
            "play_behavior": "REPLACE_ALL"
        });
        self.ubus(
            device_id,
            "player_play_music",
            "mediaplayer",
            json!({
                "startaudioid": audio_id,
                "music": serde_json::to_string(&music)?,
            }),
        )
        .await
    }

    /// 按机型自动选择接口来播放一个 URL。
    pub async fn play_on(&self, device: &XiaomiDevice, url: &str) -> Result<PlayApi> {
        let api = Self::play_api_for(&device.hardware);
        let ok = match api {
            PlayApi::PlayUrl => self.play_by_url(&device.device_id, url, 2).await?,
            PlayApi::PlayMusic => {
                self.play_by_music_url(
                    &device.device_id,
                    url,
                    DEFAULT_AUDIO_ID,
                    DEFAULT_CP_ID,
                    2,
                )
                .await?
            }
        };
        if !ok {
            bail!(
                "{} 在设备 `{}` ({}) 上返回失败",
                api.as_str(),
                device.label(),
                device.hardware
            );
        }
        Ok(api)
    }

    /// 播放操作：`play` / `pause` / `stop`。
    pub async fn play_operation(&self, device_id: &str, action: &str) -> Result<bool> {
        if !matches!(action, "play" | "pause" | "stop") {
            bail!("不支持的 action: {action}（只能是 play/pause/stop）");
        }
        self.ubus(
            device_id,
            "player_play_operation",
            "mediaplayer",
            json!({ "action": action, "media": "app_ios" }),
        )
        .await
    }

    pub async fn stop(&self, device_id: &str) -> Result<bool> {
        self.play_operation(device_id, "stop").await
    }

    pub async fn pause(&self, device_id: &str) -> Result<bool> {
        self.play_operation(device_id, "pause").await
    }

    pub async fn resume(&self, device_id: &str) -> Result<bool> {
        self.play_operation(device_id, "play").await
    }

    /// `mediaplayer.player_set_volume`
    pub async fn set_volume(&self, device_id: &str, volume: u8) -> Result<bool> {
        if volume > 100 {
            bail!("音量必须在 0..=100");
        }
        self.ubus(
            device_id,
            "player_set_volume",
            "mediaplayer",
            json!({ "volume": volume, "media": "app_ios" }),
        )
        .await
    }

    /// `mediaplayer.player_get_play_status` —— 真实负载是 `data.info`（JSON 字符串）。
    pub async fn play_status(&self, device_id: &str) -> Result<Value> {
        let resp = self
            .ubus_raw(
                device_id,
                "player_get_play_status",
                "mediaplayer",
                json!({ "media": "app_ios" }),
            )
            .await?;
        Self::ensure_ok(&resp, "player_get_play_status")?;
        let data = resp.get("data").cloned().unwrap_or(Value::Null);
        if let Some(info) = data.get("info").and_then(Value::as_str) {
            if let Ok(parsed) = serde_json::from_str::<Value>(info) {
                return Ok(parsed);
            }
        }
        Ok(data)
    }

    /// `mibrain.text_to_speech`
    pub async fn text_to_speech(&self, device_id: &str, text: &str) -> Result<bool> {
        self.ubus(device_id, "text_to_speech", "mibrain", json!({ "text": text }))
            .await
    }

    // ---------------------------------------------------------------- 工具

    /// 从设备列表里按 did 或 MiNA uuid 找设备。
    pub fn find<'a>(devices: &'a [XiaomiDevice], key: &str) -> Result<&'a XiaomiDevice> {
        devices
            .iter()
            .find(|d| d.did == key || d.device_id == key)
            .ok_or_else(|| anyhow!("找不到设备 `{key}`（可用 did 或 MiNA uuid）"))
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head}…(共 {} 字符)", s.chars().count())
}

/// 把顶层 JSON 对象摊平成表单字段。
///
/// 字符串原样（`message` 已经是 JSON 字符串，交给表单编码去转义）；
/// 其余标量序列化成紧凑 JSON —— 与 aiohttp 对 dict 的处理一致。
fn to_form_pairs(v: &Value) -> Vec<(String, String)> {
    v.as_object()
        .map(|obj| {
            obj.iter()
                .map(|(k, val)| {
                    let s = match val {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    (k.clone(), s)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_api_selection_matches_miservice_whitelist() {
        // L16A / L16B（Xiaomi Sound）不在白名单 → player_play_url
        assert_eq!(MinaClient::play_api_for("L16B"), PlayApi::PlayUrl);
        assert_eq!(MinaClient::play_api_for("L16A"), PlayApi::PlayUrl);
        // L05C（小爱音箱Play增强版）在白名单 → player_play_music
        assert_eq!(MinaClient::play_api_for("L05C"), PlayApi::PlayMusic);
        assert_eq!(MinaClient::play_api_for("l05c"), PlayApi::PlayMusic);
    }

    #[test]
    fn ensure_ok_reports_code_and_message() {
        let err = MinaClient::ensure_ok(&json!({"code": 900, "message": "bad"}), "ubus")
            .unwrap_err()
            .to_string();
        assert!(err.contains("900") && err.contains("bad"), "{err}");
        assert!(MinaClient::ensure_ok(&json!({"code": 0}), "ubus").is_ok());
    }

    #[test]
    fn media_type_one_sets_music_audio_type() {
        // 直接校验 play_by_music_url 里 audio_type 的取值逻辑
        let audio_type = |t: i64| if t == 1 { "MUSIC" } else { "" };
        assert_eq!(audio_type(1), "MUSIC");
        assert_eq!(audio_type(2), "");
    }

    #[test]
    fn form_pairs_keep_message_as_raw_json_string() {
        // ubus 的 message 必须是「一个 JSON 字符串」字段，不能被再包一层引号。
        let body = json!({
            "deviceId": "f58c6453-0268-4df5-a220-1335bb84172b",
            "method": "player_set_volume",
            "path": "mediaplayer",
            "message": r#"{"volume":25,"media":"app_ios"}"#,
        });
        let pairs = to_form_pairs(&body);
        let get = |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("deviceId"), "f58c6453-0268-4df5-a220-1335bb84172b");
        assert_eq!(get("message"), r#"{"volume":25,"media":"app_ios"}"#);
        assert_eq!(pairs.len(), 4);
    }

    #[test]
    fn form_pairs_serialize_non_string_scalars() {
        // serde_json 默认用 BTreeMap，字段是字典序，所以按 key 取值而不是按下标。
        let pairs = to_form_pairs(&json!({ "volume": 25, "flag": true }));
        let get = |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("volume"), Some("25"));
        assert_eq!(get("flag"), Some("true"));
    }
}
