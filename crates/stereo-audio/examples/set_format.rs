//! 端点默认格式探针：读 / 设 / 还原。
//!
//! ```text
//! cargo run -q -p stereo-audio --example set_format -- <endpoint-id|default> [rate channels bits [float]]
//! ```
//!
//! 不带格式参数时只打印当前格式。

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use stereo_audio::{
        default_render_format, get_endpoint_format, set_endpoint_format, EndpointFormat,
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    let target = args.first().map(String::as_str).unwrap_or("default");

    let (id, before): (String, EndpointFormat) = if target == "default" {
        default_render_format()?
    } else {
        (target.to_string(), get_endpoint_format(target)?)
    };
    println!("endpoint: {id}");
    println!("before : {}", before.describe());

    if args.len() < 4 {
        return Ok(());
    }
    let rate: u32 = args[1].parse()?;
    let channels: u16 = args[2].parse()?;
    let bits: u16 = args[3].parse()?;
    let is_float = args.get(4).map(|s| s == "float").unwrap_or(false);
    let after = set_endpoint_format(&id, rate, channels, bits, is_float)?;
    println!("after  : {}", after.describe());
    let reread = get_endpoint_format(&id)?;
    println!("reread : {}", reread.describe());
    println!("same   : {}", reread == after);
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    println!("windows only");
}
