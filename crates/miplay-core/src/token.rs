//! 小米账号凭据。
//!
//! 文件格式与 [MiService](https://github.com/Yonsm/MiService) 和 MiXPlay 兼容：
//!
//! ```json
//! {
//!   "deviceId": "2895…",
//!   "userId": "2319…",
//!   "passToken": "V1:…",
//!   "micoapi": ["<ssecurity>", "<serviceToken>"]
//! }
//! ```
//!
//! ⚠️ 这个文件里是**明文**凭据，绝不能提交进 git。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 小米云服务 id —— 小爱音箱相关的所有调用都用它。
pub const SID_MICOAPI: &str = "micoapi";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiToken {
    #[serde(rename = "deviceId", default)]
    pub device_id: String,

    #[serde(rename = "userId", default)]
    pub user_id: String,

    #[serde(rename = "passToken", default)]
    pub pass_token: Option<String>,

    /// 其余字段原样保留，典型是 `{"micoapi": ["<ssecurity>", "<serviceToken>"]}`
    #[serde(flatten)]
    pub services: HashMap<String, Value>,
}

impl MiToken {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取凭据失败: {}", path.display()))?;
        let token: MiToken = serde_json::from_str(&text)
            .with_context(|| format!("凭据不是合法 JSON: {}", path.display()))?;
        if token.user_id.is_empty() {
            bail!("凭据缺少 userId: {}", path.display());
        }
        Ok(token)
    }

    /// 取某个服务 id 对应的 `(ssecurity, service_token)`。
    pub fn service(&self, sid: &str) -> Option<(&str, &str)> {
        let arr = self.services.get(sid)?.as_array()?;
        Some((arr.first()?.as_str()?, arr.get(1)?.as_str()?))
    }

    pub fn has_service(&self, sid: &str) -> bool {
        self.service(sid).is_some()
    }

    pub fn cookie_header(&self, sid: &str) -> Result<String> {
        let (_, st) = self.service(sid).with_context(|| {
            format!("凭据里没有服务 `{sid}` 的 serviceToken —— 需要重新登录小米账号")
        })?;
        Ok(format!("userId={}; serviceToken={}", self.user_id, st))
    }

    /// 脱敏后的表示，用于日志与前端展示。**不含任何密钥明文。**
    pub fn redacted(&self) -> Value {
        let services: HashMap<&str, String> = self
            .services
            .iter()
            .map(|(k, v)| {
                let len = v.as_array().map(|a| a.len()).unwrap_or(0);
                (k.as_str(), format!("<{len} 个密钥字段>"))
            })
            .collect();
        json!({
            "deviceId": mask(&self.device_id),
            "userId": mask(&self.user_id),
            "passToken": self.pass_token.as_ref().map(|_| "<已保存>"),
            "services": services,
        })
    }

    /// 按优先级列出默认候选路径。
    pub fn default_paths() -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(p) = std::env::var("MIPLAY_TOKEN_FILE") {
            if !p.trim().is_empty() {
                out.push(PathBuf::from(p));
            }
        }
        if let Ok(p) = std::env::var("MIPLAY_TOKEN") {
            if !p.trim().is_empty() {
                out.push(PathBuf::from(p));
            }
        }
        if let Ok(appdata) = std::env::var("APPDATA") {
            out.push(PathBuf::from(&appdata).join("miplay-bridge").join(".mi.token"));
        }
        if let Ok(home) = std::env::var("USERPROFILE") {
            // MiXPlay 的默认配置目录
            out.push(PathBuf::from(&home).join("conf").join(".mi.token"));
        }
        if let Ok(home) = std::env::var("HOME") {
            out.push(PathBuf::from(home).join(".config").join("miplay-bridge").join(".mi.token"));
        }
        out
    }

    /// 找到第一个存在的默认凭据文件。
    pub fn find_default() -> Result<PathBuf> {
        let mut tried = Vec::new();
        for p in Self::default_paths() {
            if p.is_file() {
                return Ok(p);
            }
            tried.push(p.display().to_string());
        }
        bail!(
            "找不到小米账号凭据文件，已尝试:\n  {}\n\
             可以用 MIPLAY_TOKEN_FILE 环境变量指定路径。",
            tried.join("\n  ")
        )
    }

    /// 从默认位置加载。
    pub fn load_default() -> Result<Self> {
        Self::load(Self::find_default()?)
    }
}

fn mask(s: &str) -> String {
    let n = s.chars().count();
    if n <= 8 {
        return "*".repeat(n);
    }
    let head: String = s.chars().take(4).collect();
    let tail: String = s.chars().skip(n - 4).collect();
    format!("{head}****{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MiToken {
        serde_json::from_str(
            r#"{"deviceId":"2895177471A8F1","userId":"23198271",
                "passToken":"V1:secret","micoapi":["ssec","stoken"]}"#,
        )
        .unwrap()
    }

    #[test]
    fn reads_service_tuple() {
        let t = sample();
        assert_eq!(t.service(SID_MICOAPI), Some(("ssec", "stoken")));
        assert!(t.has_service(SID_MICOAPI));
        assert!(!t.has_service("other"));
        assert_eq!(
            t.cookie_header(SID_MICOAPI).unwrap(),
            "userId=23198271; serviceToken=stoken"
        );
    }

    #[test]
    fn redacted_hides_secrets() {
        let text = sample().redacted().to_string();
        assert!(!text.contains("secret"), "脱敏后仍泄露 passToken: {text}");
        assert!(!text.contains("stoken"), "脱敏后仍泄露 serviceToken: {text}");
        assert!(!text.contains("ssec"), "脱敏后仍泄露 ssecurity: {text}");
        assert!(text.contains("2895****A8F1"));
    }
}
