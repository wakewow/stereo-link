//! 让在线的音箱用「小爱同学」的嗓子说一句话——用来单独验证**音箱喇叭本身是否出声**。
//!
//! 这条路只走小米云 `text_to_speech`，与我们自己的采集/推流/AirPlay 全部无关；
//! 如果连它都没声音，问题就在音箱侧（静音、音量、未通电、不在附近），而不在链路里。
//!
//! ```text
//! cargo run -p miplay-server --example say -- "测试一二三"
//! cargo run -p miplay-server --example say -- "测试" 501718792
//! ```

use anyhow::Result;
use miplay_core::{MiToken, MinaClient};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let text = args.next().unwrap_or_else(|| "测试一二三".to_string());
    let only: Vec<String> = args.collect();

    let token = MiToken::load_default()?;
    let client = MinaClient::new(token)?;
    let devices = client.device_list().await?;
    println!("在线设备 {} 台：", devices.len());
    let mut hit = 0usize;
    for dev in &devices {
        let id = if dev.did.is_empty() {
            dev.device_id.clone()
        } else {
            dev.did.clone()
        };
        let label = dev.label().to_string();
        println!("  - {label} (did={}) online={}", dev.did, dev.online());
        if !only.is_empty() && !only.iter().any(|k| k == &id) {
            continue;
        }
        if !dev.online() {
            println!("    跳过：离线");
            continue;
        }
        match client.text_to_speech(&dev.device_id, &text).await {
            Ok(ok) => {
                println!("    TTS -> {ok}");
                hit += 1;
            }
            Err(e) => println!("    TTS 失败: {e}"),
        }
    }
    println!("\n已下发给 {hit} 台设备，文本：{text:?}");
    Ok(())
}
