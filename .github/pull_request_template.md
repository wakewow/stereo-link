## 改了什么

<!-- 一两句话概括；如果是 UI 改动请附截图（PrintWindow 抓窗口本身）。 -->

## 为什么

<!-- 关联 issue：Fixes #… -->

## 怎么验证的

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace --lib`
- [ ] 真机验证（贴命令 + 关键输出/数字；涉及音频链路请给前后对比）

## 检查项

- [ ] 没有提交本机专有信息（端点 id / 设备序列号 / 蓝牙地址 / 个人目录 / 令牌）
- [ ] 默认配置里没有写死具体设备名
- [ ] 新增/修改的公开 API 有文档注释
- [ ] 含中文的文件是 UTF-8（`.ps1` / `.cmd` 为 ASCII-only 或带 BOM）
