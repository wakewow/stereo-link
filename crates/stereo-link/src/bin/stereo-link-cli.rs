//! 控制台版入口（`stereo-link-cli.exe`）：真正的逻辑都在 `stereo_link::cli` 里。
//!
//! 单独一个 exe（而不是给 GUI 加参数）是为了管道可用：窗口子系统的进程
//! 拿不到父进程的 stdout，`stereo-link-cli status --json | ConvertFrom-Json` 会拿到空。

fn main() {
    std::process::exit(stereo_link::cli::main());
}
