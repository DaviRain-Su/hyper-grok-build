# Terminal Support and Troubleshooting

Grok Build runs as a full-screen TUI. To draw the interface, it relies on terminal escape sequences for color, clipboard, mouse, and full-screen control. Some terminals, multiplexers, and SSH sessions handle these sequences differently.

## Quick Fixes

### Truecolor / Washed-out or wrong colors

```bash
# Add to ~/.zshrc or ~/.bashrc
export COLORTERM=truecolor
```

Inside tmux or over SSH, also add to your tmux config:

```tmux
# ~/.tmux.conf or ~/.byobu/.tmux.conf
set -g default-terminal "tmux-256color"
set -as terminal-features ",*:RGB"
```

### Recommended tmux settings (clipboard + passthrough)

```tmux
set -g set-clipboard on
set -g allow-passthrough on
```

After editing, run:

```bash
tmux source-file ~/.tmux.conf
# or detach and reattach
```

### Terminal diagnostics

Run a read-only report from your shell without starting the TUI:

```bash
grok doctor
grok doctor --json  # machine-readable report
```

The command reports the terminal, multiplexer, **color level**, **available themes**, the same compact **Clipboard** preflight status used by `/doctor`, and—when this build can capture audio—the **microphone** it would open. It also lists detected issues, recommendations, and probes that could not run. It exits successfully when it produces a report, even when the report contains issues or recommendations. Color detection uses stderr or the controlling terminal rather than stdout, so `grok doctor --json | jq` reports the same terminal capabilities as direct output. Passive mic lookup does not open a stream and cannot detect a denied macOS microphone grant.

Inside Grok, run the read-only `/doctor`. It uses the same diagnostic facts and clipboard policy, with runtime-only evidence such as the current screen mode, Kitty keyboard negotiation, and XTVERSION replies. When voice mode is on, it also shows the Voice section. Standalone doctor points to `/doctor` only for live-TUI evidence; skipped tmux and other external probes remain separate unavailable notes. When color is below truecolor, both reports explain how to unlock truecolor-only themes (TokyoNight, RosePineMoon, OscuraMidnight), or note that Terminal.app is inherently 256-color. The permanent aliases `/terminal-setup`, `/terminal-check`, and `/terminal-info` run the same slash command.

---

## Detected Terminals

Grok detects these terminal emulators from environment variables:

- **Apple Terminal** (Terminal.app)
- **Ghostty**
- **iTerm2**
- **Warp**
- **WezTerm**
- **Kitty**
- **Alacritty**
- **Rio**
- **foot** (Wayland-native, Linux)
- **VS Code**, **Cursor**, **Windsurf**, and **Zed** integrated terminals
- **JetBrains** IDE terminals (IntelliJ, PhpStorm, and others)
- **Grok Desktop**
- **VTE**-based terminals (GNOME Terminal, GNOME Console, Tilix)
- **Windows Terminal**

Detection has these limitations:

- Inside tmux, the variables Grok needs to identify the terminal don't reach the pager.
- Over SSH, many terminal variables aren't forwarded.
- tmux's global environment (`tmux -g`) reflects the first client that attached to the server, not your current session.

---

## Common Problems and Fixes

### Problem: Colors look wrong or lack truecolor

**Cause**: `COLORTERM` not set or tmux not configured for 24-bit RGB.

**Fix**: Apply the two settings above, then restart Grok.

**Verify**: Run `/doctor`. Expect `color truecolor` and `themes all`. If `color` is `256` or `basic`, the issues section has the unlock fix.

### Problem: Clipboard problems

Grok writes to the clipboard through up to three routes, shown in the **Clipboard** section of `/doctor`:

- **native** — Grok always writes to the native OS clipboard first.
- **tmux buffer** — inside tmux, Grok also writes to the tmux paste buffer (`tmux load-buffer`).
- **OSC 52** — Grok emits the OSC 52 escape sequence so the outer terminal updates its clipboard. Grok always emits OSC 52 inside tmux. Outside tmux, it emits OSC 52 on Linux, over SSH, or in a container without a display.

**Linux Wayland**: on compositors that support the data-control protocol (GNOME 48+, KDE, Sway, Hyprland — the **Clipboard** section shows `data-control on`; the line is omitted off Wayland) copies work even if the terminal loses focus mid-copy. On older compositors (GNOME 46/47), keep the terminal focused until the copy toast confirms, and install the `wl-clipboard` package (provides `wl-copy`) for the most reliable route — Grok shows a startup warning when this applies. If data-control misbehaves on your compositor, set `GROK_CLIPBOARD_NO_DATA_CONTROL=1` to stop Grok from speaking that protocol entirely — copies then go through the CLI tools (`wl-copy`/`xclip`).

**OSC 52 kill switch**: Grok emits OSC 52 on every Linux copy (and over SSH/tmux/containers). Terminals that do not implement OSC 52 may paint the base64 payload as visible garbage (for example some VNC/X11 clients such as OpenText Exceed). Set `GROK_CLIPBOARD_NO_OSC52=1` before starting Grok to force the OSC 52 leg off; `/doctor` then shows `osc 52 off`. Native and tmux clipboard legs are unchanged.

**Linux X11 selections**: X11 **PRIMARY** and **CLIPBOARD** are separate. Selecting text usually fills PRIMARY; an explicit Copy action fills CLIPBOARD. In Grok:

- An unmodified middle click reads PRIMARY only when `DISPLAY` is non-empty. Pure X11 can fall back to the native arboard reader. XWayland must have `xclip` or `xsel` on `PATH`; Grok deliberately disables the arboard fallback there so it cannot substitute Wayland PRIMARY.
- `Ctrl+V` reads CLIPBOARD only and never falls back to PRIMARY. To fill CLIPBOARD from a shell, run `printf %s "text" | xclip -selection clipboard`.
- `Shift+Insert` remains the terminal-native selected-text paste. Native Wayland PRIMARY behavior is compositor/terminal-specific and is not inferred from `TERM` or an incoming mouse event.

**SSH and selected text**: a remote Grok process usually cannot read the local terminal's PRIMARY or CLIPBOARD selection. Use terminal-native `Shift+Insert`, or hold `Shift` while middle-clicking when your terminal uses that gesture to bypass mouse reporting. The terminal then sends the local selection through the PTY instead of asking the remote process to access it.

**Unknown terminals over SSH**: when Grok cannot identify the outer terminal, it sends the copy but reports delivery as unverified. If paste fails, reconnect with `grok wrap <ssh command>` or use `/minimal`.

**Known limitation — Apple Terminal + SSH**:
Apple Terminal ignores OSC 52, so copying from a Grok session over SSH can't reach your local clipboard. Grok writes every in-app copy to a backup file (`~/.grok/last-copy.txt`, override with `GROK_COPY_FILE`) and the toast names the path — so you can `cat`/`scp` it. You can also target a file explicitly with `/copy out.txt` or `/copy 2 ~/reply.md`. For native drag-select copy (terminal selection → local clipboard), turn mouse capture off with `/toggle-mouse-reporting` (opt-in feature) or run `grok --minimal`.

**Optional workaround for live clipboard**: Use `grok wrap ssh` instead of plain `ssh` (for example, `grok wrap ssh user@host`). It runs the command in a local PTY that intercepts OSC 52 sequences, including tmux-wrapped ones, and writes their contents to your local clipboard. The same command wraps anything else whose clipboard can't reach you — for example `grok wrap docker exec -it <container> bash` or `grok wrap kubectl exec -it <pod> -- bash`.

`grok wrap` also protects your local terminal from dirty disconnects: if the wrapped command dies while a remote TUI has mouse reporting, the alternate screen, or similar modes enabled (for example the SSH connection drops mid-session), wrap resets those modes on exit instead of leaving the terminal spraying mouse escape codes.

When Grok starts inside an SSH session that isn't already running under `grok wrap`, a one-time contextual tip above the prompt recommends `grok wrap ssh <host>` (it stops appearing on its own once you launch through wrap). To turn it off, set `ssh_wrap = false` under `[ui.contextual_hints]` in `~/.grok/config.toml`, or use `/settings` → **Show contextual hints** → **SSH wrap**.

For repeated use, run `grok doctor fix ssh-wrap` on your **local machine**. Canonical `terminal.ssh-wrap` remains accepted and appears in JSON. After showing the exact change and asking for confirmation, it adds an interactive-shell alias to `~/.bashrc`, `~/.zshrc`, or `~/.config/fish/config.fish`. Automatic setup is unavailable on Windows. The safety scan refuses direct `ssh` alias/function declarations in that target file only; aliases from sourced files, plugins, or dynamic shell setup require manual review before confirming. Use `command ssh ...` to bypass the alias. For manually typed `ssh -f`, ControlPersist workflows, or OpenSSH's `~^Z` local suspend, use the bypass because wrapping is not fully transparent for those cases.

> **Warning**: `grok wrap` is **experimental** and may misbehave in some setups.

**iTerm2 setting**:
iTerm2 requires explicit permission for OSC 52:

1. iTerm2 → **Settings** → **General** → **Selection**
2. Enable **"Applications in terminal may access clipboard"**

This setting is off by default for security reasons. Without it, OSC 52 writes from Grok (or any TUI) will be ignored.

**Fix for other cases**:
- `set -g set-clipboard on` in tmux config
- For other terminals over SSH, switch to iTerm2, Ghostty, WezTerm, or Kitty for native OSC 52 support

### Problem: Fullscreen / alternate screen not activating (inline mode)

**Cause**: Zellij, tmux control mode (`tmux -CC`), or config set to `never`.

**Fix**:
- In Zellij or control mode, Grok intentionally runs inline (no alt screen).
- Set `[terminal] alt_screen = "always"` in `~/.grok/pager.toml` to force fullscreen.
- Use the CLI flag `--no-alt-screen` to disable alt-screen mode entirely (useful for debugging or when the alternate screen causes issues in your terminal).

### Problem: Zellij keybindings interfere with Grok (Ctrl+g, Ctrl+o, etc.)

Zellij intercepts many Ctrl/Alt key combinations before they reach full-screen TUIs like Grok.

**Best fix** (Zellij 0.41+): Switch to the **"Unlock-First (non-colliding)"** preset:

1. Press `Ctrl+o` → `c` (open Configuration)
2. Go to **"Change Mode Behavior"**
3. Select **"Unlock-First (non-colliding)"**
4. Press `Enter` (or `Ctrl+a` to save permanently)

After this, Zellij starts **locked**. Most keys pass through to Grok. Press `Ctrl+g` to temporarily unlock Zellij when you need its pane/session management.

In minimal mode, if `Ctrl+G` still does not reach Grok, open the command palette and select **Edit Prompt in External Editor**. This preserves the current draft; typing `/edit-prompt` starts an empty editor draft because the command itself occupies the composer.

Zellij recommends this approach for TUI users.

### Problem: `Ctrl+Enter` doesn't interject in WezTerm

**Cause**: WezTerm ships with the Kitty keyboard protocol disabled. Grok relies on it to tell `Ctrl+Enter` (interject) and `Shift+Enter` (send in multiline mode) apart from plain `Enter`. Most other terminals enable the protocol when Grok requests it.

For the same reason, in Apple Terminal, Grok binds `Ctrl+O` to interject.

**Fix**:

Add this after `config = wezterm.config_builder()` in `~/.config/wezterm/wezterm.lua`:

```lua
config.enable_kitty_keyboard = true
```

Reload (`Cmd+Shift+R` or restart WezTerm) and restart `grok`.

**Verify**: Run `/doctor` inside Grok. While a turn is active, you see the interject hint, and `Ctrl+Enter` interjects.

**Quick workaround** (no global change):

```lua
table.insert(config.keys, {
  key = "Enter",
  mods = "CTRL",
  action = wezterm.action.SendString("\x1b[13;5u"),
})
```

### Problem: `Shift+Enter` doesn't insert a newline in VS Code

**Cause**: VS Code's integrated terminal (and the Cursor / Windsurf / Zed
forks) use xterm.js, which only partially implements the Kitty keyboard
protocol — it mis-encodes shifted printable keys (`!@#$%^&*()` arrive as
plain digits). Grok therefore never negotiates the protocol for these
terminals. Without it, xterm.js sends a bare `CR` for `Shift+Enter`,
byte-for-byte identical to plain `Enter`, so the chord can't be told apart
and the prompt submits.

This also affects VS Code reached **over SSH** (e.g. into a devbox or
container): `TERM_PROGRAM` isn't forwarded, so Grok sees an `Unknown`
terminal and skips the protocol for the same reason.

**Fix**: Use **`Alt+Enter`** to insert a newline. xterm.js delivers it
reliably as `ESC`+`CR` regardless of the keyboard protocol, and Grok's
prompt hint bar advertises `Alt+Enter: newline` whenever it detects this
situation. Run `/doctor` to confirm — the `newline` row shows
`Alt+Enter` when `Shift+Enter` is unavailable.

### Problem: Mouse scrolling stops working (native scrollbar takes over)

If Grok's mouse-driven scrolling stops responding and your terminal falls back to its native scrollbar, mouse reporting is off.

**Apple Terminal**: Go to **View > Allow Mouse Reporting** (keyboard shortcut `Cmd+R`) to re-enable it. A checkmark appears next to the option when active.

**iTerm2**: Open **Settings** (`Cmd+,`) → **Profiles** → **Terminal** → ensure **"Enable mouse reporting"** is checked. Alternatively, restart iTerm2.

### Problem: Voice dictation records nothing

You start voice (`/voice` or `Ctrl+Space`), talk, and no words appear. After ~10 seconds Grok stops and shows a toast with the cause:

- **"microphone delivered only silence"** — the mic opens but delivers essentially zero audio. On macOS this is almost always microphone permission: the OS feeds unauthorized apps silence instead of erroring, and the permission belongs to the *terminal app* hosting Grok (Ghostty, iTerm2, …), not Grok itself. Open **System Settings → Privacy & Security → Microphone**, enable your terminal, and **restart the terminal**. If access is already allowed, check the input device and level under **System Settings → Sound → Input** (a fully muted or dead input can look the same; residual noise may instead show the “heard audio” toast).
- **"heard audio but no speech was detected"** — audio is flowing, so the mic path is open; speak into the selected device, or try again.

**Verify**: Run `grok doctor` or `/terminal-setup` (with voice mode on). The **Voice** section shows the microphone Grok would capture from. Neither can detect a *denied permission* passively — macOS only reveals that once recording starts (the toast above).

### Problem: Byobu + GNU screen

Byobu on screen has best-effort support only. Prefer Byobu on tmux.

---

## Still Stuck?

Run `/feedback` to report it.