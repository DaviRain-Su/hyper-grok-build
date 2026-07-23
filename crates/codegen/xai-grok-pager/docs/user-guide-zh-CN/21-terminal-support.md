# 终端支持与故障排除

Grok Build 以全屏 TUI 运行。为绘制界面，它依赖终端转义序列实现颜色、剪贴板、鼠标与全屏控制。部分终端、复用器与 SSH 会话对这些序列的处理方式不同。

## 快速修复

### Truecolor / 颜色发灰或错误

```bash
# Add to ~/.zshrc or ~/.bashrc
export COLORTERM=truecolor
```

在 tmux 内或通过 SSH 时，还需在 tmux 配置中添加：

```tmux
# ~/.tmux.conf or ~/.byobu/.tmux.conf
set -g default-terminal "tmux-256color"
set -as terminal-features ",*:RGB"
```

### 推荐的 tmux 设置（剪贴板 + 透传）

```tmux
set -g set-clipboard on
set -g allow-passthrough on
```

编辑后运行：

```bash
tmux source-file ~/.tmux.conf
# or detach and reattach
```

### 终端诊断

在 shell 中运行只读报告，无需启动 TUI：

```bash
grok doctor
grok doctor --json  # machine-readable report
```

该命令会报告终端、复用器、**颜色级别**、**可用主题**、与 `/doctor` 相同的紧凑 **Clipboard** 预检状态，以及——当本构建可采集音频时——将打开的 **麦克风**。它还会列出检测到的问题、建议，以及未能运行的探测。只要成功生成报告就会以成功状态退出，即使报告中包含问题或建议。颜色检测使用 stderr 或控制终端而非 stdout，因此 `grok doctor --json | jq` 报告的终端能力与直接输出一致。被动麦克风查询不会打开流，也无法检测 macOS 上被拒绝的麦克风授权。

在 Grok 内运行只读的 `/doctor`。它使用相同的诊断事实与剪贴板策略，并附带仅运行时可得的证据，例如当前屏幕模式、Kitty 键盘协商与 XTVERSION 回复。开启语音模式时还会显示 Voice 部分。独立 doctor 仅在需要实时 TUI 证据时指向 `/doctor`；被跳过的 tmux 及其他外部探测仍作为单独的不可用说明保留。当颜色低于 truecolor 时，两份报告都会说明如何解锁仅 truecolor 可用的主题（TokyoNight、RosePineMoon、OscuraMidnight），或注明 Terminal.app 本质上是 256 色。永久别名 `/terminal-setup`、`/terminal-check` 与 `/terminal-info` 运行同一斜杠命令。

---

## 检测到的终端

Grok 根据环境变量检测以下终端模拟器：

- **Apple Terminal**（Terminal.app）
- **Ghostty**
- **iTerm2**
- **Warp**
- **WezTerm**
- **Kitty**
- **Alacritty**
- **Rio**
- **foot**（Wayland 原生，Linux）
- **VS Code**、**Cursor**、**Windsurf** 与 **Zed** 集成终端
- **JetBrains** IDE 终端（IntelliJ、PhpStorm 等）
- **Grok Desktop**
- **VTE** 系终端（GNOME Terminal、GNOME Console、Tilix）
- **Windows Terminal**

检测有以下限制：

- 在 tmux 内，Grok 用于识别终端的变量无法到达 pager。
- 通过 SSH 时，许多终端变量不会被转发。
- tmux 的全局环境（`tmux -g`）反映的是首个连接到服务器的客户端，而非你当前的会话。

---

## 常见问题与修复

### 问题：颜色看起来不对或缺少 truecolor

**原因**：未设置 `COLORTERM`，或 tmux 未配置 24 位 RGB。

**修复**：应用上文两项设置，然后重启 Grok。

**验证**：运行 `/doctor`。预期为 `color truecolor` 与 `themes all`。若 `color` 为 `256` 或 `basic`，issues 部分会包含解锁修复说明。

### 问题：剪贴板问题

Grok 通过最多三条路径写入剪贴板，显示在 `/doctor` 的 **Clipboard** 部分：

- **native** — Grok 始终先写入原生操作系统剪贴板。
- **tmux buffer** — 在 tmux 内，Grok 还会写入 tmux 粘贴缓冲区（`tmux load-buffer`）。
- **OSC 52** — Grok 发出 OSC 52 转义序列，以便外层终端更新其剪贴板。在 tmux 内 Grok 始终发出 OSC 52。在 tmux 外，它会在 Linux、SSH 或无显示的容器中发出 OSC 52。

**Linux Wayland**：在支持 data-control 协议的合成器上（GNOME 48+、KDE、Sway、Hyprland — **Clipboard** 部分显示 `data-control on`；非 Wayland 时省略该行），即使终端在复制中途失去焦点，复制也能工作。在较旧的合成器上（GNOME 46/47），请保持终端聚焦直到复制 toast 确认，并安装 `wl-clipboard` 包（提供 `wl-copy`）以获得最可靠路径 — 适用时 Grok 会显示启动警告。若 data-control 在你的合成器上行为异常，设置 `GROK_CLIPBOARD_NO_DATA_CONTROL=1` 可让 Grok 完全停止使用该协议 — 复制将改为走 CLI 工具（`wl-copy`/`xclip`）。

**OSC 52 总开关**：Grok 在每次 Linux 复制时（以及 SSH/tmux/容器场景）都会发出 OSC 52。未实现 OSC 52 的终端可能将 base64 载荷显示为可见乱码（例如某些 VNC/X11 客户端，如 OpenText Exceed）。在启动 Grok 前设置 `GROK_CLIPBOARD_NO_OSC52=1` 可强制关闭 OSC 52 路径；`/doctor` 随后会显示 `osc 52 off`。native 与 tmux 剪贴板路径不受影响。

**Linux X11 选择区**：X11 的 **PRIMARY** 与 **CLIPBOARD** 是分开的。选中文本通常填充 PRIMARY；显式的复制操作填充 CLIPBOARD。在 Grok 中：

- 未修改的中键点击仅在 `DISPLAY` 非空时读取 PRIMARY。纯 X11 可回退到原生 arboard 读取器。XWayland 必须在 `PATH` 上有 `xclip` 或 `xsel`；Grok 故意在该场景禁用 arboard 回退，以免错误替代 Wayland PRIMARY。
- `Ctrl+V` 仅读取 CLIPBOARD，且从不回退到 PRIMARY。要从 shell 填充 CLIPBOARD，运行 `printf %s "text" | xclip -selection clipboard`。
- `Shift+Insert` 仍是终端原生的“已选文本粘贴”。原生 Wayland PRIMARY 行为取决于合成器/终端，不会从 `TERM` 或传入的鼠标事件推断。

**SSH 与已选文本**：远端 Grok 进程通常无法读取本地终端的 PRIMARY 或 CLIPBOARD 选择。请使用终端原生的 `Shift+Insert`，或在终端用该手势绕过鼠标报告时，按住 `Shift` 再中键点击。此时终端会通过 PTY 发送本地选择内容，而不是要求远端进程去访问它。

**SSH 上的未知终端**：当 Grok 无法识别外层终端时，它仍会发送复制，但报告投递为未验证。若粘贴失败，请用 `grok wrap <ssh command>` 重新连接，或使用 `/minimal`。

**已知限制 — Apple Terminal + SSH**：
Apple Terminal 忽略 OSC 52，因此从 SSH 上的 Grok 会话复制无法到达本地剪贴板。Grok 会将每次应用内复制写入备份文件（`~/.grok/last-copy.txt`，可用 `GROK_COPY_FILE` 覆盖），toast 会显示该路径 — 因此你可以 `cat`/`scp` 它。也可以用 `/copy out.txt` 或 `/copy 2 ~/reply.md` 显式指定目标文件。对于原生拖选复制（终端选择 → 本地剪贴板），用 `/toggle-mouse-reporting` 关闭鼠标捕获（可选功能），或运行 `grok --minimal`。

**实时剪贴板的可选变通**：使用 `grok wrap ssh` 代替普通 `ssh`（例如 `grok wrap ssh user@host`）。它在本地 PTY 中运行命令，拦截 OSC 52 序列（包括经 tmux 包装的），并将其内容写入本地剪贴板。同一命令也可包装其他剪贴板无法到达本机的场景 — 例如 `grok wrap docker exec -it <container> bash` 或 `grok wrap kubectl exec -it <pod> -- bash`。

`grok wrap` 还会保护本地终端免受脏断开影响：若被包装的命令在远端 TUI 仍启用鼠标报告、备用屏幕或类似模式时退出（例如 SSH 连接中途断开），wrap 会在退出时重置这些模式，而不是让终端不断输出鼠标转义码。

当 Grok 在尚未运行于 `grok wrap` 的 SSH 会话中启动时，提示符上方会出现一次性上下文提示，建议使用 `grok wrap ssh <host>`（一旦通过 wrap 启动，该提示会自行停止出现）。要关闭它，在 `~/.grok/config.toml` 的 `[ui.contextual_hints]` 下设置 `ssh_wrap = false`，或使用 `/settings` → **Show contextual hints** → **SSH wrap**。

若需反复使用，在**本机**运行 `grok doctor fix ssh-wrap`。规范名称 `terminal.ssh-wrap` 仍被接受，并会出现在 JSON 中。在显示确切变更并请求确认后，它会向 `~/.bashrc`、`~/.zshrc` 或 `~/.config/fish/config.fish` 添加交互式 shell 别名。Windows 上无法自动设置。安全扫描仅拒绝目标文件中直接声明的 `ssh` 别名/函数；来自 sourced 文件、插件或动态 shell 设置的别名需在确认前手动审查。使用 `command ssh ...` 可绕过别名。对于手动输入的 `ssh -f`、ControlPersist 工作流，或 OpenSSH 的 `~^Z` 本机挂起，请使用绕过方式，因为包装对这些情况并不完全透明。

> **警告**：`grok wrap` 为**实验性**功能，在某些环境中可能表现异常。

**iTerm2 设置**：
iTerm2 需要显式授权 OSC 52：

1. iTerm2 → **Settings** → **General** → **Selection**
2. 启用 **"Applications in terminal may access clipboard"**

该设置出于安全原因默认关闭。未启用时，来自 Grok（或任何 TUI）的 OSC 52 写入会被忽略。

**其他情况的修复**：
- 在 tmux 配置中设置 `set -g set-clipboard on`
- 对于其他通过 SSH 的终端，改用 iTerm2、Ghostty、WezTerm 或 Kitty 以获得原生 OSC 52 支持

### 问题：全屏 / 备用屏幕未激活（内联模式）

**原因**：Zellij、tmux 控制模式（`tmux -CC`），或配置设为 `never`。

**修复**：
- 在 Zellij 或控制模式下，Grok 会有意以内联方式运行（无备用屏幕）。
- 在 `~/.grok/pager.toml` 中设置 `[terminal] alt_screen = "always"` 以强制全屏。
- 使用 CLI 标志 `--no-alt-screen` 可完全禁用备用屏幕模式（便于调试，或在备用屏幕导致终端问题时使用）。

### 问题：Zellij 键绑定干扰 Grok（Ctrl+g、Ctrl+o 等）

Zellij 会在许多 Ctrl/Alt 组合键到达全屏 TUI（如 Grok）之前拦截它们。

**最佳修复**（Zellij 0.41+）：切换到 **"Unlock-First (non-colliding)"** 预设：

1. 按 `Ctrl+o` → `c`（打开 Configuration）
2. 进入 **"Change Mode Behavior"**
3. 选择 **"Unlock-First (non-colliding)"**
4. 按 `Enter`（或 `Ctrl+a` 永久保存）

之后 Zellij 以**锁定**状态启动。大多数按键会透传给 Grok。需要 Zellij 的窗格/会话管理时，按 `Ctrl+g` 临时解锁。

在 minimal 模式下，若 `Ctrl+G` 仍无法到达 Grok，打开命令面板并选择 **Edit Prompt in External Editor**。这会保留当前草稿；输入 `/edit-prompt` 会启动空的编辑器草稿，因为命令本身占用了输入区。

Zellij 向 TUI 用户推荐此做法。

### 问题：`Ctrl+Enter` 在 WezTerm 中无法插话

**原因**：WezTerm 默认禁用 Kitty 键盘协议。Grok 依赖该协议区分 `Ctrl+Enter`（插话）与 `Shift+Enter`（多行模式下发送）和普通 `Enter`。大多数其他终端在 Grok 请求时会启用该协议。

同理，在 Apple Terminal 中，Grok 将 `Ctrl+O` 绑定为插话。

**修复**：

在 `~/.config/wezterm/wezterm.lua` 的 `config = wezterm.config_builder()` 之后添加：

```lua
config.enable_kitty_keyboard = true
```

重新加载（`Cmd+Shift+R` 或重启 WezTerm）并重启 `grok`。

**验证**：在 Grok 内运行 `/doctor`。在 turn 进行中应看到插话提示，且 `Ctrl+Enter` 可插话。

**快速变通**（无需全局更改）：

```lua
table.insert(config.keys, {
  key = "Enter",
  mods = "CTRL",
  action = wezterm.action.SendString("\x1b[13;5u"),
})
```

### 问题：`Shift+Enter` 在 VS Code 中无法插入换行

**原因**：VS Code 的集成终端（以及 Cursor / Windsurf / Zed
分支）使用 xterm.js，它对 Kitty 键盘
协议仅有部分实现 — 会错误编码带 Shift 的可打印键（`!@#$%^&*()` 会变成
普通数字）。因此 Grok 绝不会为这些
终端协商该协议。没有协议时，xterm.js 对 `Shift+Enter` 发送裸 `CR`，
与普通 `Enter` 逐字节相同，因而无法区分该组合键，
提示会被提交。

这也影响**通过 SSH** 连接的 VS Code（例如进入 devbox 或
容器）：`TERM_PROGRAM` 不会被转发，因此 Grok 看到的是 `Unknown`
终端，并以相同原因跳过该协议。

**修复**：使用 **`Alt+Enter`** 插入换行。xterm.js 会可靠地将其
作为 `ESC`+`CR` 传递，与键盘协议无关，且 Grok 的
提示栏在检测到此情况时会显示 `Alt+Enter: newline`。运行 `/doctor` 确认 —
当 `Shift+Enter` 不可用时，`newline` 行会显示
`Alt+Enter`。

### 问题：鼠标滚动失效（原生滚动条接管）

若 Grok 的鼠标驱动滚动停止响应，终端回退到原生滚动条，说明鼠标报告已关闭。

**Apple Terminal**：前往 **View > Allow Mouse Reporting**（快捷键 `Cmd+R`）重新启用。启用时选项旁会出现勾选标记。

**iTerm2**：打开 **Settings**（`Cmd+,`）→ **Profiles** → **Terminal** → 确保勾选 **"Enable mouse reporting"**。也可重启 iTerm2。

### 问题：语音听写没有记录任何内容

你启动语音（`/voice` 或 `Ctrl+Space`）、说话，但没有出现文字。约 10 秒后 Grok 停止，并用 toast 显示原因：

- **"microphone delivered only silence"** — 麦克风已打开，但几乎没有音频。在 macOS 上这几乎总是麦克风权限问题：操作系统对未授权应用喂静音而非报错，且权限属于托管 Grok 的*终端应用*（Ghostty、iTerm2 等），而非 Grok 本身。打开 **System Settings → Privacy & Security → Microphone**，启用你的终端，并**重启终端**。若访问已允许，请检查 **System Settings → Sound → Input** 下的输入设备与电平（完全静音或损坏的输入表现可能相同；若有残余噪声，则可能显示 “heard audio” toast）。
- **"heard audio but no speech was detected"** — 音频正在流入，说明麦克风通路已通；请对着所选设备说话，或再试一次。

**验证**：运行 `grok doctor` 或 `/terminal-setup`（开启语音模式）。**Voice** 部分会显示 Grok 将从中采集的麦克风。二者都无法被动检测*被拒绝的权限* — macOS 仅在开始录音时才会暴露（见上方 toast）。

### 问题：Byobu + GNU screen

Byobu 在 screen 上仅有尽力支持。请优先使用 Byobu on tmux。

---

## 仍然卡住？

运行 `/feedback` 进行反馈。
