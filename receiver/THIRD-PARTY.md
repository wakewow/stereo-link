# 接收端第三方组件与许可（THIRD-PARTY）

`receiver/` 是独立 workspace，产出两个独立可执行文件。它们**不静态链接进主程序**，遵循各自的许可：

| 产物 | 链接的上游 | 许可 | 说明 |
| --- | --- | --- | --- |
| `airplay-recv` | [`shairplay`](https://crates.io/crates/shairplay) 0.10 | **LGPL-3.0-or-later** | 独立 exe 分发，用户可自行替换/重链接该库（源码见 crates.io） |
| `miplay-recv` | FusionPlay MiPlay SDK 1.0.4（`fusionplay-miplay-sdk`） | **AGPL-3.0-only** | 独立 exe 分发；本仓库 `receiver/vendor/fusionplay/` 是**已修改副本**（延迟参数环境变量化），改动清单见 `vendor/README.md` |

许可全文随发布包附带：`LICENSES/LGPL-3.0-or-later.txt`、`LICENSES/AGPL-3.0-only.txt`。

## 对应源码

- `shairplay`：<https://crates.io/crates/shairplay>（crate 内包含完整源码，`cargo vendor` 可取）。
- FusionPlay：<https://github.com/rosienosiesie/FusionPlay-Android>（固定提交 `b43fe1130c3fdb233dcb41c2da174b858eb44cc2`），本仓库的修改副本见 `receiver/vendor/fusionplay/`。

## 其它依赖

两个 crate 的其余依赖均为主流宽松许可（MIT / Apache-2.0 / BSD / Zlib / MPL-2.0 等），
`cargo license` / `cargo deny` 可复核；`shairplay` 的 AP2 特性会额外引入 `symphonia`（MPL-2.0，文件级 copyleft，静态链接允许）。
