<div align="center">

<h1>Hyper(<code>hyper</code>)</h1>

<img src="docs/assets/hyper-banner.jpg" alt="Hyper —— 终端 AI 编码代理" width="720">

<p>
  <a href="https://github.com/DaviRain-Su/hyper-grok-build/releases"><img src="https://img.shields.io/github/v/release/DaviRain-Su/hyper-grok-build?display_name=tag" alt="发布版本"></a>
  <a href="https://github.com/DaviRain-Su/hyper-grok-build/actions/workflows/release.yml"><img src="https://github.com/DaviRain-Su/hyper-grok-build/actions/workflows/release.yml/badge.svg" alt="发布 CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="许可证"></a>
  <img src="https://img.shields.io/badge/rust-1.92.0-orange?logo=rust" alt="Rust 1.92">
  <img src="https://img.shields.io/badge/platform-macOS%20%C2%B7%20Linux%20%C2%B7%20Windows-lightgrey" alt="平台:macOS、Linux、Windows">
  <a href="https://github.com/DaviRain-Su/hyper-grok-build/releases"><img src="https://img.shields.io/github/downloads/DaviRain-Su/hyper-grok-build/total?label=downloads" alt="下载量"></a>
  <img src="https://img.shields.io/badge/i18n-10%20locales-brightgreen" alt="i18n:10 种语言">
</p>

**Hyper** 是 [Grok Build](https://github.com/xai-org/grok-build) 的非官方多供应商社区构建版本 ——
一个用 Rust 编写的终端 AI 编码代理,对多家 LLM 供应商提供一流支持:
xAI Grok、Kimi Code / Moonshot、ChatGPT Codex、OpenAI、Anthropic、Z.AI、Ollama Cloud 等。

它以全屏 TUI 的形式运行,能够理解你的代码库、编辑文件、
执行 shell 命令、搜索网页,并管理长时间运行的任务 ——
既可以在终端中交互使用,也可以无头模式用于脚本/CI,
还能通过 Agent Client Protocol(ACP)嵌入到编辑器中。
UI 已本地化为 10 种语言(English、中文、日本語、한국어、Español、
Português、Français、Deutsch、Русский),并可在设置中实时切换。运行
`hyper dashboard --web` 可以打开本地只读的纯 Rust Web 仪表盘，查看会话指标、
事件时间线、图表、日志和实时事件流。

[安装](#安装) ·
[供应商](#供应商) ·
[从源码构建](#从源码构建) ·
[发布](#发布) ·
[与官方 <code>grok</code> 共存](#与官方-grok-共存) ·
[许可证](#许可证)

**English: [README.md](README.md)** ·
**中文用户指南: [docs/user-guide-zh-CN/](crates/codegen/xai-grok-pager/docs/user-guide-zh-CN/)**

</div>

---

## 界面截图

真实 TUI(在 PTY 中用仓库内置的
[`tui_shot`](crates/codegen/xai-grok-pager-pty-harness/examples/tui_shot.rs)
工具捕获),展示 10 种 UI 语言中的两种:

| English | 简体中文 |
| ------- | -------- |
| ![Hyper TUI in English](docs/assets/screenshot-welcome-en.png) | ![中文界面的 Hyper TUI](docs/assets/screenshot-welcome-zh.png) |

---

## 为什么叫 "Hyper"?

本 fork 仓库已经命名为 `hyper-grok-build`,**Hyper** 沿用了这个品牌:

| | 官方版本 | 本 fork |
|---|---|---|
| 产品 | Grok Build | **Hyper** |
| 二进制文件 | `grok` | **`hyper`** |
| 安装目录 | `~/.grok` | **`~/.hyper`**(仅二进制) |
| 配置 / 认证 | `~/.grok` | **`~/.grok`**(共享;同一运行时) |
| 上游 | [xai-org/grok-build](https://github.com/xai-org/grok-build) | 多供应商社区补丁 |

简短的 CLI 名称,不与 `grok` 冲突,也为超越单一供应商留出了发展空间
(不像 [Kigi](https://github.com/ZacharyZhang-NY/Kigi-CLI) 这样只支持 Kimi 的 fork)。

---

## 安装

适用于 macOS(arm64/x86_64)、Linux(arm64/x86_64,
glibc / `linux-gnu` —— 适用于 Omarchy、Ubuntu、Fedora 等)以及 Windows
(x86_64)的预编译单文件二进制已发布在
[GitHub Releases](https://github.com/DaviRain-Su/hyper-grok-build/releases):

```sh
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash
```

```powershell
# Windows PowerShell
irm https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.ps1 | iex
```

```sh
hyper --version
hyper login          # xAI / Grok 会话(浏览器 OAuth)
hyper                # 启动 TUI
```

安装指定版本:

```sh
curl -fsSL https://raw.githubusercontent.com/DaviRain-Su/hyper-grok-build/dev/install.sh | bash -s -- --version v0.2.110
```

安装程序会根据发布版的 `SHA256SUMS` 校验每一次下载,
安装到 `~/.hyper/bin/hyper`(Windows 上为 `%USERPROFILE%\.hyper\bin\hyper.exe`),
并在需要时打印出需要添加的 PATH 配置行。

> 如需尚未发布的改动，可从下方源码构建；否则请安装上方的最新发布版。

---

## 供应商

Hyper 保留了本代码树中的多供应商注册表(见 pager
[用户指南（中文）](crates/codegen/xai-grok-pager/docs/user-guide-zh-CN/)):

| 平台 | 认证方式 | 备注 |
| -------- | ---- | ----- |
| xAI / Grok | `hyper login`(OIDC)或 `XAI_API_KEY` | 第一方模型 |
| Kimi Code | 设备 OAuth / 订阅 | `kimi-code/*` 目录 |
| Moonshot CN / AI | API key | 开放平台 |
| ChatGPT Codex | ChatGPT OAuth | GPT-5.x reasoning，并支持实验性全双工 `/live` 语音 |
| OpenAI / Anthropic / DeepSeek 风格 | API keys | BYOK 目录 |
| Z.AI Coding Plan | 平台 key | 国际版方案 |
| Ollama Cloud | API key | 实时模型清单同步 |

选择器中的模型 id 形如 `{platform}/{model}`(例如
`kimi-code/k3`、`codex:gpt-5.5`)。各平台文档位于
`crates/codegen/xai-grok-pager/docs/user-guide-zh-CN/`(Moonshot、Kimi Code、
OpenAI Codex 等)。

配置和凭据仍然存放在 **`~/.grok`**(与上游 Grok Build 相同的路径),
因此已有的会话、API key 和 `auth.json` 可以继续使用。

---

## 从源码构建

环境要求:

- **Rust** —— 由 [`rust-toolchain.toml`](rust-toolchain.toml) 锁定版本
  (首次构建时 `rustup` 会自动安装)
- **[DotSlash](https://dotslash-cli.com)** —— 密封的 `bin/protoc`
  ```sh
  cargo install dotslash
  # 或者:brew install dotslash
  ```
- **CMake 3.5+** —— 构建实验性 `/live` 语音使用的内置静态 Opus；工作区已固定
  `CMAKE_POLICY_VERSION_MINIMUM=3.5`

```sh
cargo run -p xai-grok-pager-bin              # 构建并启动 TUI(二进制名:hyper)
cargo build -p xai-grok-pager-bin --profile release-dist
./target/release-dist/hyper --version
```

组合根包仍然是 `xai-grok-pager-bin`(monorepo 布局);
**发布的二进制名称**是 `hyper`。

---

## 更新日志

发布说明见 [`CHANGELOG.md`](./CHANGELOG.md)。已知限制见:
[`docs/KNOWN_ISSUES.md`](./docs/KNOWN_ISSUES.md)。

---

## 发布

1. 将根目录的 [`VERSION`](VERSION) 文件设置为 **monorepo 锁步客户端版本**
   (与 `crates/codegen/xai-grok-pager/Cargo.toml` /
   `xai-grok-version` 保持一致,当前为 `0.2.110`)。CI 会把它编译进
   `x-grok-client-version`;xAI 会拒绝低于 **0.1.202** 的客户端(HTTP 426)。
   **不要**自己编造一个较低的营销版本号(例如 `0.1.0`)。
2. 在 `dev`(或你的发布分支)上提交;更新 `CHANGELOG.md`。
3. 打标签并推送 —— CI 会构建五个目标平台并发布 GitHub Release:

```sh
VERSION=$(tr -d '[:space:]' < VERSION)
git tag "v${VERSION}"
git push origin "v${VERSION}"
```

工作流:[`.github/workflows/release.yml`](.github/workflows/release.yml)

构建产物:

| 产物 | 示例 |
| ----- | ------- |
| macOS arm64 | `hyper-0.2.110-aarch64-apple-darwin.tar.gz` |
| macOS x86_64 | `hyper-0.2.110-x86_64-apple-darwin.tar.gz` |
| Linux x86_64(glibc) | `hyper-0.2.110-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64(glibc) | `hyper-0.2.110-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `hyper-0.2.110-x86_64-pc-windows-msvc.zip` |
| 校验和 | `SHA256SUMS` |

标签必须与 `VERSION` 完全一致(`v0.2.110` ↔ `0.2.110`),否则构建会失败。

---

## 与官方 `grok` 共存

Hyper 与 xAI / SpaceXAI **没有隶属关系**。在同一台机器上:

| 项目 | 官方 `grok` | Hyper |
|---------|-----------------|-------|
| 二进制 | `grok` | `hyper` |
| 托管安装目录 | `~/.grok/bin` | `~/.hyper/bin` |
| 配置 / 认证 / 会话 | `~/.grok` | **相同的** `~/.grok` |
| Leader IPC(`leader*.sock` / `.lock`) | 位于 `~/.grok` | **相同的**命名空间 |

注意事项:

- 会话、API key 和 OAuth 权限是共享的 —— 登录一次,两个 CLI 都能看到。
- Leader 的 list/kill 可以同时看到两个产品的 leader。请只 kill 你自己启动的 leader。
- 社区构建版禁用了上游的自更新功能,因此 `hyper update` 不会覆盖 `~/.grok/bin/grok`。升级 Hyper 请重新运行 `install.sh` / `install.ps1`。

Hyper 的安装脚本不会改写官方安装程序的任何内容。

---

## 构建说明(本 fork)

```sh
# 默认启用 community-build(Hyper 品牌 + 禁用上游更新器)。
cargo run -p xai-grok-pager-bin

# 显式构建发布风格的本地二进制
cargo build -p xai-grok-pager-bin --profile release-dist --features community-build
```

Amp 风格的 **agent 模式**(low / medium / high / ultra 档位)目前**仅有设计文档** ——
见 [`docs/design-modes.md`](docs/design-modes.md),尚未发布。

已知问题和剩余工作:[`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md)。

---

## 文档

仓库内用户指南(示例中可能仍写着 `grok`;Hyper 的二进制名是
`hyper`,路径仍在 `~/.grok` 下):

- 中文：[用户指南（中文）](crates/codegen/xai-grok-pager/docs/user-guide-zh-CN/)
- English：[User Guide](crates/codegen/xai-grok-pager/docs/user-guide/)

相关扩展文档：

- [Hooks 与 Plugins 指南（中文）](crates/codegen/xai-grok-pager/docs/hooks-and-plugins.zh-CN.md)
- [自定义 Hooks 指南（中文）](crates/codegen/xai-grok-pager/docs/custom-hooks.zh-CN.md)

上游产品文档:[docs.x.ai/build](https://docs.x.ai/build/overview)

`SOURCE_REV` 记录了本代码树最近一次同步的 monorepo 提交。

---

## 仓库结构

| 路径 | 内容 |
|------|----------|
| `crates/codegen/xai-grok-pager-bin` | 组合根;构建 `hyper` 二进制 |
| `crates/codegen/xai-grok-pager` | TUI |
| `crates/codegen/xai-grok-shell` | Agent 运行时 |
| `install.sh` / `install.ps1` | 发布安装脚本 |
| `.github/workflows/release.yml` | 多平台发布 CI |

> [!IMPORTANT]
> 根目录的 `Cargo.toml`(workspace 成员 / 依赖版本)是从 monorepo
> **生成**的 —— 请将其视为只读。本地修改请编辑各个 crate 自己的
> `Cargo.toml`,这样在同步时才能保留下来。

---

## 许可证

Apache-2.0。见 [`LICENSE`](LICENSE)、[`NOTICE`](NOTICE) 和
[`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES)。

基于 Grok Build 开源项目
([xai-org/grok-build](https://github.com/xai-org/grok-build))。
