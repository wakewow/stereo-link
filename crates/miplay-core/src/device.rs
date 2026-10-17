//! 小米设备模型。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 一台小米设备，来自 `GET /admin/v2/device_list`。
///
/// 字段名保持小米云的原始拼写（`deviceID` / `miotDID`），避免转换出错。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XiaomiDevice {
    /// MiNA uuid —— `ubus` 调用里的 `deviceId` 用它。
    #[serde(rename = "deviceID", default)]
    pub device_id: String,

    /// 米家 did。
    #[serde(rename = "miotDID", default)]
    pub did: String,

    #[serde(default)]
    pub name: String,

    /// 硬件代号，例如 `L16A` / `L16B` / `L05C`。
    #[serde(default)]
    pub hardware: String,

    #[serde(default)]
    pub model: Option<String>,

    #[serde(default)]
    pub mac: Option<String>,

    #[serde(default)]
    pub presence: Option<String>,

    #[serde(rename = "isOnline", default)]
    pub is_online: Option<bool>,

    /// 能力位，例如 `{"stereo_mode_v2": true, "yunduantts": true}`
    #[serde(default)]
    pub capabilities: Option<Value>,

    /// 云端返回的完整原始条目，前端展示与排错用。
    #[serde(default, skip_deserializing)]
    pub raw: Value,
}

impl XiaomiDevice {
    /// 从云端返回的原始 JSON 条目构造。
    pub fn from_value(v: Value) -> Self {
        let mut dev: XiaomiDevice =
            serde_json::from_value(v.clone()).unwrap_or_else(|_| XiaomiDevice::empty());
        dev.raw = v;
        dev
    }

    fn empty() -> Self {
        Self {
            device_id: String::new(),
            did: String::new(),
            name: String::new(),
            hardware: String::new(),
            model: None,
            mac: None,
            presence: None,
            is_online: None,
            capabilities: None,
            raw: Value::Null,
        }
    }

    /// 在线判断：兼容云端几种不同的字段写法。
    pub fn online(&self) -> bool {
        if let Some(b) = self.is_online {
            return b;
        }
        if let Some(p) = &self.presence {
            return p.eq_ignore_ascii_case("online");
        }
        for key in ["presence", "isOnline", "online", "status"] {
            match self.raw.get(key) {
                Some(Value::Bool(b)) => return *b,
                Some(Value::String(s)) if s.eq_ignore_ascii_case("online") => return true,
                Some(Value::Number(n)) if n.as_i64() == Some(1) => return true,
                _ => {}
            }
        }
        false
    }

    /// 这台设备是否必须走 `player_play_music`（而不是 `player_play_url`）。
    ///
    /// 依据 `miservice/minaservice.py:11-15` 的白名单。
    pub fn needs_play_music(&self) -> bool {
        crate::mina::PLAY_MUSIC_HARDWARE
            .iter()
            .any(|h| h.eq_ignore_ascii_case(&self.hardware))
    }

    pub fn capability(&self, key: &str) -> bool {
        // 云端的能力位**是数字 1/0**，不是布尔 —— 用 as_bool() 会全部判成 false。
        match self.capabilities.as_ref().and_then(|c| c.get(key)) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_i64().is_some_and(|i| i != 0),
            Some(Value::String(s)) => {
                !s.is_empty() && s != "0" && !s.eq_ignore_ascii_case("false")
            }
            _ => false,
        }
    }

    /// 是否参与了立体声组合（L16A/L16B 都是 `stereo_mode_v2`）。
    pub fn stereo_mode(&self) -> bool {
        self.capability("stereo_mode_v2") || self.capability("stereo_mode")
    }

    /// 前端 / 列表接口用的精简视图。
    ///
    /// 刻意**不含 `raw`** —— 云端每个设备条目有几十 KB，原样返回会让列表响应上兆。
    pub fn to_view(&self) -> Value {
        serde_json::json!({
            "name": self.label(),
            "did": self.did,
            "device_id": self.device_id,
            "hardware": self.hardware,
            "mac": self.mac,
            "online": self.online(),
            "stereo_mode": self.stereo_mode(),
            "play_api": crate::mina::MinaClient::play_api_for(&self.hardware).as_str(),
            "rom_version": self.raw.get("romVersion").and_then(Value::as_str),
        })
    }

    /// 用于稳定排序 / 展示的标识。
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            self.device_id.clone()
        } else {
            self.name.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_l16b_entry() {
        let d = XiaomiDevice::from_value(json!({
            "deviceID": "0c9041b3-a155-4ffb-814e-497a266b1e93",
            "miotDID": "934947888",
            "name": "Xiaomi Sound",
            "hardware": "L16B",
            "mac": "90:FB:5D:0F:B9:5C",
            "presence": "online",
            "capabilities": { "stereo_mode_v2": true }
        }));
        assert_eq!(d.did, "934947888");
        assert_eq!(d.name, "Xiaomi Sound");
        assert!(d.online());
        assert!(d.capability("stereo_mode_v2"));
        // L16B 不在 player_play_music 白名单里
        assert!(!d.needs_play_music());
        assert!(!d.raw.is_null());
    }

    #[test]
    fn l05c_needs_play_music() {
        let d = XiaomiDevice::from_value(json!({
            "deviceID": "9bc127eb-438e-42c3-a448-912f6ae38c8c",
            "miotDID": "931892755",
            "name": "小爱音箱Play增强版",
            "hardware": "L05C"
        }));
        assert!(d.needs_play_music());
        assert!(!d.online(), "没有在线字段时应判定为离线");
    }

    #[test]
    fn online_detection_variants() {
        for v in [json!({"isOnline": true}), json!({"presence": "online"}), json!({"online": 1})] {
            let mut o = v.as_object().unwrap().clone();
            o.insert("deviceID".into(), json!("x"));
            assert!(XiaomiDevice::from_value(Value::Object(o)).online(), "{v} 应判在线");
        }
    }

    #[test]
    fn capability_accepts_numeric_flags_from_cloud() {
        // 真实云端返回的是 1/0，不是 true/false —— 这是最容易踩的坑。
        let d = XiaomiDevice::from_value(json!({
            "deviceID": "x",
            "hardware": "L16A",
            "capabilities": { "stereo_mode_v2": 1, "airplay2_reprovision": 1, "no_such": 0 }
        }));
        assert!(d.capability("stereo_mode_v2"), "数字 1 必须算 true");
        assert!(d.stereo_mode());
        assert!(d.capability("airplay2_reprovision"));
        assert!(!d.capability("no_such"));
        assert!(!d.capability("missing"));
    }

    #[test]
    fn view_is_snake_case_and_excludes_raw() {
        let d = XiaomiDevice::from_value(json!({
            "deviceID": "0c9041b3-a155-4ffb-814e-497a266b1e93",
            "miotDID": "934947888",
            "name": "Xiaomi Sound",
            "hardware": "L16B",
            "mac": "90:FB:5D:0F:B9:5C",
            "presence": "online",
            "romVersion": "1.80.2",
            "capabilities": { "stereo_mode_v2": 1 },
            "deviceProfile": "should-not-leak"
        }));
        let v = d.to_view();
        // 前端读的是这几个名字
        assert_eq!(v["did"], "934947888");
        assert_eq!(v["device_id"], "0c9041b3-a155-4ffb-814e-497a266b1e93");
        assert_eq!(v["online"], true);
        assert_eq!(v["stereo_mode"], true);
        assert_eq!(v["play_api"], "player_play_url");
        assert_eq!(v["rom_version"], "1.80.2");
        // raw 与庞大字段不得进入列表视图
        assert!(v.get("raw").is_none());
        assert!(!v.to_string().contains("should-not-leak"));
    }
}
