# LICENSES/ 目录说明（provenance notes）

本目录集中存放**不是** MIT OR Apache-2.0 的那部分许可证全文，以及来源说明。

```
LICENSE-MIT                 # 项目主体（crates/ scripts/ docs/ README 等），双许可之一
LICENSE-APACHE              # 项目主体，双许可之二
NOTICE                      # 第三方署名汇总（MS-PL 驱动 + egui 字体 + 依赖说明 + 商标声明）
LICENSES/
  README.md                 # 本文件
  MS-PL.txt                 # MS-PL 全文（逐字，来自上游 Scream 的 LICENSE）
  OFL-1.1.txt               # 打包 exe/安装包时编入的 NotoEmoji 字体许可（SIL OFL 1.1）
  Ubuntu-font-1.0.txt       # 打包 exe/安装包时编入的 Ubuntu-Light 字体许可（UFL 1.0）
  Hack-MIT.txt              # 打包 exe/安装包时编入的 Hack 字体许可（MIT/Expat + Bitstream Vera）
  emoji-icon-font-MIT.txt   # 打包 exe/安装包时编入的 emoji-icon-font 许可（MIT）
```

> 上面四个字体许可文本来自构建时实际链接的 `epaint_default_fonts 0.31.1`
> （crates.io 包内 `fonts/` 目录的逐字副本），随 `stereo-link.exe` / 安装包一起分发。

## LICENSES/MS-PL.txt 的来源

- 上游：Scream，`https://github.com/duncanthrax/scream`
- 版本：commit `d789743c248b11d1df7e5ecc546b1bc60b90cd91`
- 上游许可证文件：仓库根目录 `LICENSE`（内容即 MS-PL，首行
  `The Microsoft Public License (MS-PL)`，第二行 `Copyright (c) 2015 Microsoft`）。
- 本仓库中的逐字副本（内容完全一致）：
  - `LICENSES/MS-PL.txt`（本目录）
  - `drivers/scream-derived/LICENSE-MS-PL.txt`
  - `drivers/pkg-stereolink/LICENSE-MS-PL.txt`
- **`LICENSES/MS-PL.txt` 必须是上游文件的逐字副本**。不要改写、不要翻译、不要删掉
  `Copyright (c) 2015 Microsoft` 行。上游若更换许可证文本，才同步更新，并在本文件中记录
  更新日期与新 commit。

## 哪些文件受 MS-PL 约束

| 路径 | 许可 | 说明 |
| --- | --- | --- |
| `drivers/scream-derived/**` | **MS-PL** | 从 Scream 派生的驱动源码；文件头 Microsoft/Tom Kistner 版权声明必须保留（MS-PL §3(C)） |
| `drivers/pkg-stereolink/StereoLinkVad.{inf,sys,cat}` | **MS-PL** | 上述源码的编译产物；按 MS-PL §3(D)，以「compiled or object code form」分发时必须在一份符合 MS-PL 的许可下进行——随附 LICENSE-MS-PL.txt + 本目录 MS-PL.txt 即可满足 |
| `drivers/pkg-stereolink/LICENSE-MS-PL.txt` | MS-PL 全文 | 保留原处，不要只依赖 `LICENSES/` |
| `drivers/certs/stereolink-dev.cer` | 无许可声明 | 只是开发用**公钥**证书，供复现构建/本机测试；私钥 `.pfx/.pvk/.snk` 不入库 |
| `drivers/build.ps1`、`make_package.ps1`、`sign-vad.ps1`、`install-vad.ps1`、`inf-src/**` | MIT OR Apache-2.0 | 我们新写的构建/安装脚本，不含上游代码（但产物受 MS-PL 约束） |
| `crates/**`、`scripts/**`、`docs/**`、根目录 `.cmd`、`README.md` | MIT OR Apache-2.0 | 项目主体 |
| `docs/images/*.png` | MIT OR Apache-2.0 | 自制截图/示意图 |
| `drivers/REPORT.md` | MIT OR Apache-2.0 | 我们的交接/技术报告 |

## 为什么 MS-PL 与 MIT/Apache 可以同仓库共存

- MS-PL 不是 copyleft：§2(A) 只要求派生作品保留声明，不要求「衍生作品整体」采用 MS-PL。
- MS-PL §3(D) 对源码形式的要求是「以本许可证分发，并附完整许可证副本」——所以
  `drivers/scream-derived/**` 必须保留 MS-PL，不能改成 MIT/Apache。
- 对编译形式的要求是「在一份符合 MS-PL 的许可下分发」——按 MS-PL 本身分发即可满足。
- 仓库其余部分（MIT/Apache）与 MS-PL 部分属于「聚合/不同部分」，各自许可互不传染；
  但必须让下游一眼看出分界：`NOTICE` + 本目录 + README 许可证节 + `drivers/` 内已有的
  `LICENSE-MS-PL.txt` 四处一致说明。

## 二进制发布时额外要做的

`stereo-link.exe` 用 eframe/egui 的 `default_fonts` 特性构建时，会把
`epaint_default_fonts 0.31.1` 的字体数据编进可执行文件，该 crate 的许可是
`(MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0`，包内实际含四种字体：
Ubuntu-Light（UFL 1.0）、NotoEmoji（OFL 1.1）、Hack（MIT/Expat + Bitstream Vera）、
emoji-icon-font（MIT）。因此发布 exe / 安装包时要：

1. 附 `LICENSES/OFL-1.1.txt`、`LICENSES/Ubuntu-font-1.0.txt`、`LICENSES/Hack-MIT.txt`、
   `LICENSES/emoji-icon-font-MIT.txt` 全文；
2. 附 `NOTICE`；
3. 附所有 Rust 依赖的许可清单（建议 `cargo about generate` 或 `cargo deny`）。

**本仓库已经这么做**：`installer/stereo-link.iss` 会把 `LICENSES/**`、`NOTICE`、
`LICENSE-MIT`、`LICENSE-APACHE` 一并装进 `{app}`，所以从 Release 下载的安装包自带
上述全部许可文本；只下载裸 `stereo-link.exe` 时请自行附上这些文件。

只发布**源码**（GitHub 仓库）时不需要额外附字体许可文本，因为字体是构建时从 crates.io
拉取的依赖，不是本仓库的源文件；但仓库里保留这些副本，方便打包者直接取用。
