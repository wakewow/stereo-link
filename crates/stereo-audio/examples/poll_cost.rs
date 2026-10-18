//! `poll_cost` —— 量一量 UI 线程上那些「每帧/每秒轮询」的 COM 调用到底多贵。
//!
//! 背景：面板每 ~900 ms 调一次 `default_render()`（枚举端点），每 ~400 ms / ~1 s
//! 调 `get_endpoint_volume()`，拖窗口时这些开销会直接卡住 winit 的移动消息循环。
//!
//! ```text
//! cargo run -q -p stereo-audio --example poll_cost
//! ```

use std::time::Instant;

use stereo_audio::capture::{list_endpoints, resolve_endpoint, CaptureMode};
use stereo_audio::endpoint::{DataFlow, DeviceSelector};
use stereo_audio::volume::get_endpoint_volume;

fn bench<F: FnMut()>(label: &str, n: usize, mut f: F) {
    // 预热一次，避免第一次的 COM 初始化算进去。
    f();
    let mut times = Vec::with_capacity(n);
    let mut errs = 0usize;
    for _ in 0..n {
        let t = Instant::now();
        f();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        let _ = &mut errs;
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let sum: f64 = times.iter().sum();
    println!(
        "{label:<44} n={n} 平均 {:>7.2} ms  中位 {:>7.2} ms  最小 {:>7.2}  最大 {:>7.2}",
        sum / n as f64,
        times[n / 2],
        times[0],
        times[n - 1]
    );
}

fn main() {
    let n = 20;
    println!("== UI 线程轮询成本（n={n}）==");

    let eps = list_endpoints().unwrap_or_default();
    let render: Vec<_> = eps
        .iter()
        .filter(|e| matches!(e.flow, DataFlow::Render))
        .collect();
    println!("当前 render 端点数：{}", render.len());
    let Some(first) = render.first() else {
        println!("没有 render 端点，退出");
        return;
    };
    let full_name = first.name.clone();
    let id = first.id.clone();
    println!("样例端点：{full_name}  id={id}\n");

    bench("list_endpoints()（枚举全部端点）", n, || {
        let _ = list_endpoints();
    });

    bench("resolve_endpoint(NameContains 全名)", n, || {
        let _ = resolve_endpoint(
            &DeviceSelector::NameContains(full_name.clone()),
            CaptureMode::Loopback,
        );
    });

    bench("resolve_endpoint(Id)", n, || {
        let _ = resolve_endpoint(&DeviceSelector::Id(id.clone()), CaptureMode::Loopback);
    });

    bench("get_endpoint_volume(全名子串)", n, || {
        let _ = get_endpoint_volume(&full_name);
    });

    bench("get_endpoint_volume(id)", n, || {
        let _ = get_endpoint_volume(&id);
    });
}
