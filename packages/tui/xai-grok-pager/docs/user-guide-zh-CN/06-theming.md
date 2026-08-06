# 主题与外观自定义

Grok Build 的 TUI 颜色统一来自一套中心主题。你可以在 Grok 运行时切换主题、跟随操作系统的浅色/深色外观，并通过配置文件调整回滚布局、动画与块样式。

---

## 可用主题

Grok 内置十九个主题，另有 `auto` 选项会跟随系统外观：

| 主题 | 配置名 | 说明 | 需要真彩色 |
|-------|-------------|-------------|--------------------|
| **GrokNight** | `groknight`, `grok-night`, `dark` | 中性深色底 + 品红强调色。默认主题。 | 否 |
| **GrokDay** | `grokday`, `grok-day`, `light`, `day` | 适合明亮环境的浅色主题。 | 否 |
| **TokyoNight** | `tokyonight`, `tokyo-night`, `tokyo` | Tokyo Night 深色偏蓝色板。 | 是 |
| **RosePineMoon** | `rosepine`, `rose-pine`, `rosepine-moon`, `rose-pine-moon` | 柔和深色底与淡紫强调色。 | 是 |
| **OscuraMidnight** | `oscura`, `oscura-midnight` | 深邃底色与暖色强调。 | 是 |
| **Everforest** | `everforest`, `ever-forest` | 柔和森林绿色。 | 是 |
| **Nord** | `nord` | 冷色北极灰与霜蓝强调。 | 是 |
| **Dracula** | `dracula` | 紫灰底与鲜明强调色。 | 是 |
| **Gruvbox Dark** | `gruvbox`, `gruvbox-dark` | 温暖复古棕色。 | 是 |
| **Catppuccin Mocha** | `catppuccin`, `catppuccin-mocha`, `mocha` | 柔和粉彩深色色板。 | 是 |
| **Solarized Dark** | `solarized`, `solarized-dark` | 经典 Solarized 青绿色底。 | 是 |
| **Deep Ocean** | `deep-ocean`, `deepocean`, `ocean` | 近黑海军蓝与明亮蓝色。 | 是 |
| **Ember** | `ember` | 深栗色与玫瑰/琥珀强调。 | 是 |
| **Midnight OLED** | `midnight-oled`, `oled`, `midnight` | 纯黑底与琥珀强调。 | 是 |
| **Solarized Light** | `solarized-light` | 暖奶油色浅色画布。 | 是 |
| **Catppuccin Latte** | `catppuccin-latte`, `latte` | 冷调浅灰蓝色板。 | 是 |
| **Paper** | `paper`, `sepia` | 暖色复古纸张画布。 | 是 |
| **Base16 Default Dark** | `base16-default-dark`, `base16-default`, `base16-dark`, `base16` | Chris Kempson 的经典 Base16 Default Dark 色板。 | 是 |
| **OMP Titanium** | `omp`, `titanium`, `omp-titanium` | 高对比钛金属表面与电光蓝强调色。 | 是 |

主题名不区分大小写。`auto` 选项（别名 `system`）见 [自动主题（系统外观）](#auto-theme-system-appearance)。

### 极简模式没有主题

**极简模式**（`--minimal`）始终使用单一固定的终端原生色板渲染，并完全忽略 `theme` 设置（这些设置仍作用于完整 TUI）。极简模式直接画在你终端自身的背景上，因此使用终端的默认前景/背景色及其 16 色 ANSI 色板——与 `git` 或 `ls` 相同——在任何浅色或深色终端配置下都保持可读，无需检测或配置。在极简模式下，`/theme` 以及 `/settings` 中的主题相关行均不可用。

极简模式下的语法高亮**不会**在浅色与深色主题文件之间切换（有意避免极性检测）。接近灰色的 token 继承终端默认前景色；有彩色的 token 使用基础 ANSI 强调色（红/绿/黄/蓝/品红/青），使 read-file 输出与围栏代码在浅色与深色配置下都清晰可读。

---

## 切换主题

### 在 TUI 中

运行 `/theme` 斜杠命令（别名 `/t`）打开主题选择器。用方向键在列表中移动时，Grok 会实时预览每个主题。按 Enter 应用并保存选择，或按 Escape 还原。

若不想用选择器，可直接传入名称：

```
/theme tokyonight
```

单独提交 `/theme`（不从选择器中选择）会循环切换到下一个主题。

### 通过配置文件

在 `~/.grok/config.toml` 中设置主题：

```toml
[ui]
theme = "tokyonight"
```

---

## 透明背景（磨砂玻璃效果）

默认情况下 TUI 会绘制不透明的主题背景（深色主题为黑色）。设置
`transparent_background = true` 可让所有基础背景透明化（`Color::Reset`），
让**终端模拟器自身的背景**透过整个界面显示：

```toml
[ui]
transparent_background = true
```

配合终端侧的透明设置即可实现磨砂玻璃效果：

| 终端 | 设置 |
|------|------|
| **Windows Terminal** | 设置 → 外观 → 开启 **Acrylic material**（或 **Mica**）并调低**背景不透明度** |
| **iTerm2** | 偏好设置 → Profiles → Window → 开启 **Transparency** 和 **Blur** |
| **GNOME Terminal / Konsole** | 终端配置文件 → 开启背景**透明** |

说明：

- 文字、强调色、选中高亮、diff 颜色和语法配色都会保留——只清除结构性表面背景，保证可读性。
- 适用于任何主题；编辑 `config.toml` 后重启 TUI 即可生效（无需重新编译）。
- 透明模式下模态框背后的变暗底色会消失（模态框本身仍正常渲染）。

---

## 自动主题（系统外观）

设置 `theme = "auto"` 可让 Grok 跟随操作系统的浅色/深色外观并自动切换主题：

```toml
[ui]
theme = "auto"
```

默认情况下，深色模式映射到 **GrokNight**，浅色模式映射到 **GrokDay**。可用 `auto_dark_theme` 与 `auto_light_theme` 分别覆盖：

```toml
[ui]
theme = "auto"
auto_dark_theme = "tokyonight"
auto_light_theme = "grokday"
```

`theme = "system"` 是 `theme = "auto"` 的别名。

### 检测方式

| 平台 | 方法 |
|----------|--------|
| **macOS** | 读取 `AppleInterfaceStyle` 系统偏好设置 |
| **Linux** | 查询 XDG Desktop Portal（`org.freedesktop.appearance.color-scheme`） |
| **Windows** | 读取系统个性化注册表 |
| **SSH / 无头环境** | 启动时回退为 OSC 11 终端背景查询 |

运行后，Grok 每 5 秒轮询一次外观变化。在操作系统中切换浅色/深色模式后，数秒内即可生效，无需重启。

### 通过设置面板

运行 `/settings`（别名 `/config`），打开 **Appearance** 分类，即可交互式设置 **Auto dark theme** 与 **Auto light theme**。在 `/theme` 选择器中选中 `auto` 会启用自动模式，并使用这些映射。

---

## 颜色支持检测

启动时，Grok 会检测终端的颜色能力级别：

| 级别 | 说明 | 检测方式 |
|-------|-------------|-----------|
| **真彩色**（24-bit） | 完整 RGB 颜色。所有主题按设计渲染。 | `COLORTERM=truecolor` 或等效终端能力 |
| **256 色** | 索引色板。RGB 值映射到最近的色板项。 | 标准 xterm-256color |
| **16 色** | 仅 ANSI 名称。颜色映射到最接近的 ANSI 色。 | 基础终端支持 |

设置 `NO_COLOR` 时，Grok 不输出颜色，以单色渲染。

运行 `/doctor` 可查看检测到的级别（`color` 行）以及此终端上选择器会提供的主题（`themes` 行）。缺少真彩色时，issues 部分会说明如何启用（或说明 Terminal.app 无法启用）。

### 自动量化

每个主题都以完整 RGB 值定义。启动时，Grok 会将所有颜色量化到与检测到的能力级别匹配。这意味着：

- 在 **真彩色** 终端上，颜色原样通过。
- 在 **256 色** 终端上，每个 RGB 值映射到最近的索引色板项。
- 在 **16 色** 终端上，颜色映射到 ANSI 名称。

GrokNight 与 GrokDay 使用中性灰色，量化后仍然清晰。其他完整 TUI 主题依赖明确的 RGB 画布和语义层次，因此主题选择器会在非真彩色终端上隐藏它们。极简模式仍直接使用终端原生 ANSI 色板，所以终端已配置 Base16 时会自然沿用该色板。

### 运行时生成的颜色

运行时生成的颜色（语法高亮、背景混合）也会经过同一套量化流水线，确保在所有终端类型上外观一致。

---

## 光标颜色

Grok 使用 OSC 12 转义序列将终端光标设为当前主题的 `accent_user` 颜色，以标识正在进行的 Grok 会话。光标颜色会：

- 在启动时以及切换主题时应用。
- 退出时通过 OSC 112 重置为终端默认。

在支持 OSC 12 的终端中有效（大多数现代终端）。

---

## 紧凑模式

用 `/compact-mode` 斜杠命令切换紧凑模式。紧凑模式会：

- 移除外层垂直内边距（上下边距变为 0）。
- 将水平内边距减到最小（1 列）。
- 减少提示区与信息块的顶部内边距。

该设置会持久化到 `~/.grok/config.toml` 的 `[ui].compact_mode`，重启后仍然保留。

在小屏幕上使用紧凑模式可最大化内容区域。

---

## 语法高亮

Grok 内置三个用于代码块语法高亮的 `.tmTheme` 文件，并按当前主题选择其一：

- `grok-night.tmTheme` -- GrokNight 及所有深色预设（包括 Base16 Default Dark）
- `grok-day.tmTheme` -- GrokDay 及所有浅色预设
- `tokyo-night.tmTheme` -- TokyoNight

切换主题时，Grok 会自动选择对应文件。`.tmTheme` 文件内置在二进制中，无法用自定义文件替换。

---

## 使用 pager.toml 进行深度自定义

若需精细控制 TUI 外观，请创建 `~/.grok/pager.toml`。该文件控制回滚布局、块样式、动画等。所有设置都有默认值；只需指定要覆盖的值。（开发构建会生成带全部默认值注释的模板文件——取消某行注释即可覆盖；保持注释则继续跟随未来默认值。）

### 布局

控制视口内边距与块间距：

```toml
[scrollback.layout]
outer_vpad = 1          # Vertical padding (top/bottom) for the viewport
outer_hpad_left = 2     # Left margin (minimum: 1)
outer_hpad_right = 2    # Right margin (minimum: 1)
block_pad_left = 2      # Padding between accent line and content
block_pad_right = 2     # Padding after content at right edge
```

### 滚动条

```toml
[scrollback.scrollbar]
enabled = true          # Show/hide the scrollbar
gap_left = 0            # Gap between content and scrollbar (0 = adjacent)
gap_right = 0           # Gap between scrollbar and screen edge (0 = at edge)
# scrollbar_bg = "none" # Override background color (or "none" for theme default)
# scrollbar_fg = "none" # Override thumb color (or "none" for theme default)
```

### 滚动行为

```toml
[scrollback.scroll]
margin = 0                  # Context lines above/below selected entry (0 = edge)
min_page_fraction = 0       # Minimum scroll as % of viewport (0-100)
follow_indicator = "center" # "center" = show down-arrow, "none" = hidden
follow_auto_select = true   # Auto-select latest entry when following
follow_by_overscroll = true # Scrolling past bottom engages follow mode
anchor_on_fold = true       # Keep block header at same screen position when folding
```

### 显示选项

```toml
[scrollback.display]
sticky_headers = true              # Pin user prompts as headers when scrolled past
tab_width = 4                      # Spaces per tab character (0 = pass through)
expandable_indicator = true        # Show "›" on foldable collapsed entries
expandable_indicator_char = "›"    # Character to use (default: "›")
collapsed_accent_char = "❙"        # Accent for collapsed groupable blocks (falls back to "|" on the legacy Windows console)
dim_accent = 0.5                   # Blend factor for dimmed accents (0.0-1.0)
line_under_last_entry = false      # Horizontal line below last entry
selection_buttons = false          # Show copy/view buttons on selection box
```

### 动画

```toml
[animation]
fps = 30           # Frame rate (1-60). Higher = smoother, more CPU
wave_rows = 32     # Rows per wave cycle for accent animation
```

### 块样式：编辑 Diff

```toml
[scrollback.blocks.edit]
indent = true                   # Indent diff content
vpad = false                    # Vertical padding around diffs
# expanded_by_default = true    # Unset: follows [ui] collapsed_edit_blocks in config.toml
                                # (flag on = collapsed one-liner); uncomment to pin either shape
hunk_separator = "…"            # Separator between hunks ("…", "───", "⋯", or "" for none)
dual_line_numbers = false       # Two-column line numbers (old + new, like GitHub)
# line_summary = false          # Show +N/-M in the collapsed header; unset follows the same flag
# bg = "none"                   # Block background ("none", "light", "dark")
```

### 块样式：思考/推理

```toml
[scrollback.blocks.thinking]
accent_enabled = true       # Show accent line for thinking blocks
animate = true              # Animate accent line while thinking
truncated_lines = 3         # Lines to show in truncated mode
bg_blend = 70               # Markdown-color blend with background (0-100)
header = true               # Show "Thinking..." header
header_bright = false       # Bright header style (vs dim/muted)
```

### 块样式：工具调用

```toml
[scrollback.blocks.tool]
muted_collapsed = true     # Gray out collapsed tool calls
dim_details = true          # Dim parenthetical details (line counts, match counts)
bullet = "diamond"          # Bullet style before tool headers
```

可用的项目符号样式：

| 配置值 | 字符 | 说明 |
|-------------|-----------|-------------|
| `none` | （无） | 无项目符号 |
| `dot` | `·` | 中点（最小） |
| `small-circle` | `•` | 圆点 |
| `circle` | `●` | 实心圆 |
| `small-triangle` | `▸` | 右指小三角 |
| `triangle` | `▶` | 右指三角 |
| `diamond` | `◆` | 实心菱形（默认） |

### 块样式：执行（Shell 命令）

```toml
[scrollback.blocks.execute]
first_lines = 2                   # Output lines shown at start in truncated mode
last_lines = 3                    # Output lines shown at end in truncated mode
accent_enabled = true             # Show accent line (animated while running)
header_style = "label"            # "shell" ($ prefix) or "label" (Run prefix)
muted_command_collapsed = true    # Mute command text when collapsed
```

### 块样式：用户提示（回滚区）

```toml
[scrollback.blocks.prompt]
vpad = true            # Vertical padding
bg = "light"           # Background ("none", "light", "dark")
show_prefix = true     # Show the prompt prefix character
min_lines = 2          # Minimum content lines in truncated/sticky mode
```

### 提示输入控件

```toml
[prompt]
collapse_unfocused = true    # Collapse when scrollback is focused
mouse_hover = true           # Show hover highlight on mouse over
show_prefix = true           # Show the prompt prefix character
```

### Todo 徽章

```toml
[todo]
badge_format = "default"   # "default" = 2/5 (done/total), "colon" = [▶:1 □:4 ✓:3 ✗:2], "comma" = [1 ▶, 4 □, 3 ✓, 2 ✗]
```

### 终端行为

```toml
[terminal]
alt_screen = "auto"    # "auto", "always", or "never"
```

Alt-screen 策略：
- `auto` -- 在普通终端与普通 tmux 中全屏；在 tmux control mode 与 Zellij 中内联。
- `always` -- 始终进入全屏。
- `never` -- 从不进入全屏；在主回滚区内联运行。

### 插件 UI

```toml
disable_plugins = false   # Set to true to hide /hooks, /plugins commands and annotations
```

---

## 主题颜色槽位

每个主题定义了以下在整个 TUI 中使用的颜色槽位：

**背景：** `bg_base`、`bg_light`、`bg_dark`、`bg_highlight`、`bg_hover`、`bg_terminal`、`bg_visual`

**强调色：** `accent_user`、`accent_assistant`、`accent_thinking`、`accent_tool`、`accent_system`、`accent_error`、`accent_success`、`accent_running`、`accent_skill`、`accent_plan`、`accent_verify`、`accent_feedback`、`accent_remember`、`accent_model`

**文本：** `text_primary`、`text_secondary`

**灰色：** `gray_dim`、`gray`、`gray_bright`

**语义色：** `command`、`path`、`running`、`warning`、`fuzzy_accent`

**边框与滚动条：** `selection_border`、`hover_border`、`prompt_border`、`prompt_border_active`、`scrollbar_bg`、`scrollbar_fg`

**粘贴：** `paste_bg`、`paste_fg`、`paste_dim`

**Diff：** `diff_delete_bg`、`diff_delete_fg`、`diff_insert_bg`、`diff_insert_fg`、`diff_equal_fg`、`diff_gutter_fg`

**Markdown：** 标题颜色（`md_heading_h1`-`md_heading_h6`）、`md_code`、`md_code_bg`、`md_text`、`md_muted`、`md_task_checked`、`md_task_unchecked`、`link_fg`

主题系统在内部管理这些槽位，并自动按你的终端能力进行量化。
