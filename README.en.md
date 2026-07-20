<div align="center">

<img src="assets/Zaivern.png" width="140" alt="Zaivern Code" />

# ⚡ Zaivern Code

**A Rust-native AI Agent Cockpit for commanding Claude Code, Codex, and Gemini CLI in parallel.**

This is not a tool for writing code.<br>
**It is a cockpit for commanding a fleet of AI agents — and the development itself.**

[日本語](README.md) | [**English**](README.en.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

</div>

---

## Your bottleneck is no longer how fast you can type

Claude Code writes the implementation. Codex fixes the tests. Gemini CLI polishes the docs. That way of building software is no longer the future — it's now. And yet what you have in front of you is a pile of scattered terminal tabs.

- You can't tell at a glance which agent is running and which one has stalled
- A Claude Code session sat waiting for approval for 30 minutes — and that sinking feeling when you finally notice
- Pasting the same instruction into three tabs, three times, like it's your job

Agents don't get tired. They don't complain. **The one keeping them waiting is always the human.**

Zaivern Code was born to eliminate this friction of command. See every agent at once. Give one order to the whole fleet. Answer with one click the moment you're needed. You stop being the person who *writes* the code — and become the person who **commands the work**.

---

## 🚀 Into the cockpit in 30 seconds

**macOS / Linux** — fetches a prebuilt binary automatically (builds from source if none matches):

```bash
curl -fsSL https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.sh | sh
```

**Windows** — PowerShell:

```powershell
irm https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.ps1 | iex
```

Launch with `zai [workspace path]`. Prebuilt binaries for every OS are also available directly from [**Releases**](https://github.com/tacyan/zaivern-code/releases/latest) (macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64).

---

## The view from the cockpit

### 🎛 See the whole fleet — Agent Cockpit

Press ⌘⇧C and every running agent lines up in a grid. Each cell is not a decorative preview — it's a **live terminal you can type into directly**. Claude Code pushing the implementation forward, Codex repairing tests, Gemini CLI writing docs — the first time you watch five agents working at once on a single screen, expect goosebumps.

### 📣 One order, everyone moves — Broadcast

"Make sure the tests pass." "That approach is fine — continue." One input box, sent to every active session at the same instant. The nights of hopping between tabs pasting the same sentence are over as of today.

### 🛡 The reins stay in your hands — 3 permission modes

When you attack, use **⚡ Full-auto**: bypass flags are added to each CLI automatically, and the interactive prompts that survive even bypass mode (first-run warnings, folder-trust confirmations, plan approvals) are detected from the screen text and answered automatically — a two-layer system. When you defend, use **🛡 Approve**: any bypass flags smuggled into a command are stripped automatically, failing safe. And **🤖 Agent-first** respects whatever flags your preset says, verbatim. Switch with one click in the toolbar; push the change to already-running sessions in bulk.

**Speed and safety were never an either/or.**

### 🔔 When you're needed, you will know — notifications and your sidekick 🦀 Zaigani

The instant an agent asks for approval — a popup appears, a sound plays, the session's ● turns yellow, and **Zaigani**, the little desktop pet strolling in the corner of your screen, starts fidgeting with an "❗ approval needed" sign. A bubble floats above its head: **✔ Approve / ✖ Deny**, one click. When a run succeeds it jumps 🎉; when one fails it goes 💥 with X-eyes.

You're not staring at cold logs — **a companion taps you on the shoulder.** Development where no agent ever waits on you feels better than you'd imagine.

### 📱 Leave your desk — the command continues — Phone Remote

Tap 📱 in the top bar, scan the QR code, and any phone on the same Wi-Fi becomes your remote control. From the sofa, from the balcony, while the coffee brews — approvals, new instructions, file edits, progress checks. **While the agents are working, there is no reason left for you to be chained to a desk.** (Per-launch random token auth, LAN only.)

### 📝 And the final stroke is still yours — a Zed-inspired editor

Even in an era where AI writes 90% of the code, the last 10% — the architectural judgment calls, the naming, the one line you take responsibility for — belongs to a human. So Zaivern keeps a sharp pen right beside the commander's chair: syntect syntax highlighting, LSP diagnostics, Git diff gutters, a fuzzy palette (⌘P), VS Code-grade file operations and scrolling. **The moment you feel like writing, you can.**

---

## Why Rust — a heavy cockpit is no cockpit at all

- **No Electron. No Node.** A single native binary with GPU rendering via egui. Instant startup; idle memory lighter than one browser tab
- **A real PTY terminal** (portable-pty + vt100). Claude Code's full-screen TUI runs as-is. 256-color / TrueColor, bracketed paste, scrollback
- **One codebase for macOS / Windows / Linux.** Child processes are killed automatically on exit — no orphan processes left behind
- Lineage: **Zed's speed × Cmux's parallel agents × AGI Cockpit's pilot-seat UX**

---

## Feature reference

Everything below is the manual for each instrument on the flight deck.

### 📝 Editor
- Syntax highlighting via syntect (Rust / TS / Python / Go / Markdown and many more, auto-detected by extension)
- Tabs, line-number gutter, unsaved indicator (●), save confirmation before closing
- VS Code-grade file operations in the file tree: ➕ new file / 🗂 new folder (inline input), ✏ rename (open tabs' paths and languages follow automatically), 🗑 delete (with confirmation dialog)
- Right-click menu: open / new / rename / delete / "Send path to agent (@path)" / copy full path
- In-file search (⌘F, hit count, jump-to-hit centered on screen)
- VS Code-grade scrolling: fixed gutter, scrollBeyondLastLine, PageUp/PageDown
- Fuzzy command palette (⌘P for files, ⌘⇧P for commands)
- Git branch display, automatic Japanese UI font fallback

### 🤖 Multi-agent
- Launch agent presets with one click (⌘⇧A) and run multiple sessions in parallel
- Per-session status (●/○), uptime, restart, force-kill
- Permission modes auto-apply to presets whose command starts with `claude` / `codex` / `agy` (no flags needed in the preset). Any other CLI agent — Gemini CLI included — runs in parallel just by registering a preset
- Push permission-mode changes to running sessions via each row's 🛡 button (or "🛡 switch all")

### 🔔 Notifications + sounds
- Approval-wait, success (✅), and failure (❌ + exit code) announced via popup + OS-native sounds (can be turned off)
- When the window is unfocused, notifications also go to macOS Notification Center (Linux: notify-send)

### 🦀 Desktop pet "Zaigani"
- Blinks, follows your cursor with its eyes, wanders around; dozes off when idle → deep sleep (💤), startled hop when you come back
- Agent-linked reactions: marching "⚙ n" while agents run (faster with more agents), grooving (🎵) at 3+, fidgeting on approval-wait, 🎉 on success / 💥 on failure
- 💬 Approval bubble: ✔ Approve / ✖ Deny / Open with one click (keys sent to the PTY are customizable via `pet_approve_keys` / `pet_deny_keys`)
- Click to toggle the Cockpit (jumps to the waiting session if one exists), drag to reposition (auto-saved)
- 🎭 4 looks (blocky / crab / cat / cloud) + swap in any image you like, 📏 3 sizes

### 🔌 Plugins (build one, share it, get one)
A plugin system anyone who can write shell commands can use. A plugin is one folder under `~/.zaivern/plugins/<name>/`, declaring up to three kinds of things in `plugin.toml`:

- **▶ Commands**: run any shell command and feed the result back into the editor
  - `input` = `none` | `selection` | `file`; `output` = `replace` | `insert` | `new_tab` | `notify` | `silent`
  - Scope by language with `langs = ["rust"]`, bind a shortcut with `keybind = "cmd+alt+f"`, run automatically on save with `on_save = true` (formatter-friendly)
  - Environment variables `ZV_FILE` / `ZV_LANG` / `ZV_WORKSPACE` / `ZV_PLUGIN_DIR` are available. Runs in the background with a timeout; never overwrites a buffer you edited mid-run
- **🎨 Themes**: bundle color-theme JSON (VS Code-compatible, JSONC OK). Standalone themes in `~/.zaivern/themes/*.json` are picked up automatically
- **✂️ Snippets**: VS Code-compatible format. Type the prefix and press Tab to expand (`${1:default}` tab stops, `$0`, variables; multibyte-safe)

Three buttons to manage it all: **➕ New** (generates a full sample template), **📤 Export** (writes a `.zvplug` you can hand to anyone), **📦 Install** (just pick a received `.zvplug` / `.zip`).

```toml
# plugin.toml example: auto-format JSON on save
[plugin]
name = "json-fmt"
version = "0.1.0"
description = "Format JSON on save"

[[command]]
title = "Format JSON"
run = "python3 -m json.tool"
input = "file"
output = "replace"
langs = ["json"]
on_save = true
keybind = "cmd+alt+f"
```

### 🔤 Language servers (LSP)
If `rust-analyzer` / `typescript-language-server` / `pyright-langserver` / `gopls` is on your PATH it starts automatically and shows diagnostics (errors/warnings). The line-number gutter turns red/yellow, and the status bar shows `⛔count ⚠count`. Editing works normally even without any server installed.

Setup examples: `rustup component add rust-analyzer` / `npm i -g typescript-language-server typescript` / `npm i -g pyright` / `go install golang.org/x/tools/gopls@latest`

### ⌨️ Japanese input (IME)
Type Japanese directly inside the terminal. Uncommitted composition text is overlaid with an underline at the cursor position, and only committed text is sent to the agent.

### 🌿 Git line gutter
In a git repository, line numbers are color-coded by diff (green = added, yellow = modified). The status bar shows the branch name + changed-file count (±N).

### 💾 Session restore
On restart, the previous tabs, active tab, and panel state are restored automatically per workspace (`~/.zaivern/sessions/`).

### 📱 Phone Remote in detail
- **What you can do**: view/edit/save open files, switch tabs, search & open workspace files, view agent terminals, send instructions, approve (Enter / Esc / ^C / ↑ / ↓ / Tab / ⇧Tab / 1 / 2 / 3 / y buttons), and run commands (save, new file, Cockpit, font ±, approval-mode switch, and more)
- **How it works**: a tiny built-in HTTP server (port 8899, auto-fallback to 8900–8919 if busy). Pure `std::net` — zero extra crates
- **Security**: authenticated with a random token generated per launch (embedded in the QR URL). Tokenless API access gets a 401. LAN only

---

## Installing (manual)

The one-liners at the top are fastest. `install.sh` places a prebuilt binary from GitHub Releases at `~/.local/bin/zai`, and on platforms without a matching binary it builds from source with Rust (auto-installing rustup if needed).

- **Prebuilt binary**: grab your OS's archive from [Releases](https://github.com/tacyan/zaivern-code/releases/latest), extract `zai` (`zai.exe` on Windows), and put it somewhere on your PATH
- **From source** (requires Rust):

```bash
cargo install --git https://github.com/tacyan/zaivern-code --locked
```

Installs to `~/.cargo/bin/zai`.

### Build & run

```bash
# Requires Rust 1.88+ (rustup update stable)
cargo build --release

# Launch (pass a workspace path; defaults to the current directory)
./target/release/zai ~/dev/my-project
```

The same code builds on macOS / Windows / Linux (Linux needs rfd dependencies such as `libgtk-3-dev`).

---

## Keybindings

| Key | Action |
|---|---|
| ⌘⇧C | **Toggle Agent Cockpit** |
| ⌘⇧A | **Launch agent (preset #1)** |
| ⌘J or ⌘\` | Toggle terminal/agent panel |
| ⌘P (Ctrl+P) | Fuzzy-find and open a file |
| ⌘⇧P | Command palette (`>` prefix) |
| ⌘S / ⌘⇧S | Save / Save as |
| ⌘N / ⌘W | New file / Close tab |
| ⌘F | Find in file |
| ⌘/ | Toggle line comment |
| ⌘⇧D | Duplicate line |
| ⌥↑ / ⌥↓ | Move line up / down |
| PageUp / PageDown | Cursor + scroll by one screen |
| Enter | Auto-indent (previous line's indent, extra level after `{ ( [ :`) |
| ⌘B | Toggle sidebar |
| ⌘+ / ⌘- | Font size up / down |

On Windows / Linux, read ⌘ as Ctrl. Inside the terminal, control keys like Ctrl+C, arrows, Tab, and Esc go straight to the PTY (Shift/Option+Enter is sent as a newline, supporting Claude Code's multi-line input).

Every shortcut can be overridden in `config.toml` under `[keybindings]` (`save = "cmd+s"` format). Action names: `save` `save_as` `close_tab` `new_file` `palette_files` `palette_commands` `toggle_terminal` `toggle_sidebar` `find` `toggle_cockpit` `new_agent` `font_inc` `font_dec` `toggle_comment` `duplicate_line` `move_line_up` `move_line_down`. Modifiers: `cmd` `ctrl` `shift` `alt` (= `option`).

---

## Customization — `~/.zaivern/config.toml`

Generated automatically on first launch. After editing, run **"Reload settings"** from the command palette for instant effect (or open the file directly via **"Open config.toml"**).

```toml
# Theme: "zaivern-dark" | "zaivern-midnight" | "zaivern-light"
# or a full path to a VS Code-compatible theme JSON
theme = "zaivern-dark"
editor_font_size = 15.0
terminal_font_size = 13.0
show_hidden_files = true

# Default permission mode (auto-applied to claude / codex / agy)
#   "ask"   = user approval required every time (safe, default)
#   "auto"  = auto-YES to everything (bypass flags added per CLI)
#   "agent" = agent-first (use whatever flags the preset command says)
approval_mode = "ask"

# Desktop pet 🦀
show_pet = true
# pet_variant = "blocky"   # look: "blocky" | "crab" | "cat" | "cloud"
# pet_scale = 1.0          # size: 0.75=S / 1.0=M / 1.4=L
# pet_free_roam = true     # wanders around
# pet_sleep = true         # sleeps when idle
# pet_sounds = true        # sound effects
# pet_bubbles = true       # approval bubble
# pet_approve_keys = "\r"    # keys sent to the PTY on approve (Enter)
# pet_deny_keys = "\u001B"   # keys sent to the PTY on deny (ESC)

# ── AI agent presets (add as many as you like) ──
[[agents]]
name = "Claude Code"
icon = "🤖"
command = "claude"

[[agents]]
name = "Claude Code (full-auto)"
icon = "⚡"
command = "claude --dangerously-skip-permissions"

[[agents]]
name = "Codex"
icon = "🧠"
command = "codex"

[[agents]]
name = "Codex (full-auto)"
icon = "⚡"
command = "codex --dangerously-bypass-approvals-and-sandbox"

[[agents]]
name = "Gemini CLI"
icon = "✨"
command = "gemini"

[[agents]]
name = "Antigravity"
icon = "🚀"
command = "agy"

[[agents]]
name = "Shell"
icon = "🖥"
command = ""          # empty string = login shell

# [[agents]]
# name = "Claude (explicit Opus)"
# icon = "🧠"
# command = "claude --model claude-opus-4-8"
# env = { MAX_THINKING_TOKENS = "31999" }
```

- `command` runs through the login shell (`$SHELL -lc`), so your PATH and aliases just work.
- `env` injects preset-specific environment variables (model selection, API-key switching, etc.).
- `cwd = "~/some/dir"` pins the working directory (defaults to the workspace).
- **Per-project overrides**: drop a `.zaivern.toml` in the workspace root to set theme, fonts, approval mode, and extra agents per project.
- **Choices made in the UI are auto-saved to `~/.zaivern/state.toml`** (theme, approval mode, pet settings) — your handwritten config.toml stays clean. "Reload settings" gives config.toml priority.

### Command tricks
- Right-click in the file tree → "🤖 Send path to agent" types `@path ` (Claude Code's file-reference syntax)
- Command palette → "Send current file to agent (@path)"
- Use the Cockpit's broadcast to send the same instruction to multiple Claude Code sessions at once
- Answer approval waits from the pet's bubble with one click — or from your phone when you're away

---

## Architecture

```
src/
├── main.rs          Entry point (eframe bootstrap)
├── app.rs           App state, layout, shortcuts, palette integration
├── theme.rs         3 themes (Dark / Midnight / Light) + egui style application
├── theme_json.rs    Color-theme JSON import (VS Code-compatible)
├── config.rs        ~/.zaivern/config.toml loading, generation, project overrides
├── editor.rs        Buffer & tab management
├── editor_ops.rs    Pure text-editing operations (multibyte-safe)
├── highlight.rs     syntect → egui LayoutJob conversion (hash-cached)
├── snippets.rs      VS Code-compatible snippet parsing & Tab expansion
├── file_tree.rs     Lazy-loading file tree + context menu
├── fuzzy.rs         Fuzzy-match scoring
├── palette.rs       Command palette state & action definitions
├── keybinds.rs      Customizable keybindings
├── git.rs           git CLI integration (branch, per-line diff marks)
├── lsp.rs           Minimal LSP client (stdio JSON-RPC, diagnostics)
├── terminal.rs      PTY sessions + vt100 rendering + approval-prompt detection/auto-reply
├── agents.rs        Session management (launch/restart/destroy/broadcast/permission modes)
├── remote.rs        Phone remote (built-in HTTP server, QR code, token auth)
├── session.rs       Per-workspace session restore
├── notify.rs        OS-native notifications
├── sound.rs         Sound effects (fire-and-forget OS-standard sounds)
├── plugins.rs       Plugin system (commands/themes/snippets/.zvplug)
├── pet.rs           Desktop pet core (state machine + rendering)
├── pet_variants.rs  Pet looks (crab/cat/cloud)
└── pet_bubble.rs    Approval bubble (✔ Approve / ✖ Deny card)
```

- The terminal pipeline: PTY reader thread → `vt100::Parser` (Mutex) → per-frame cell rendering. The PTY resizes along with the window.
- Child processes are killed automatically when the app exits or a session is destroyed — no orphan processes.

## Roadmap
- [x] Keybinding customization via config.toml
- [x] Git diff gutter (color-coded line numbers)
- [x] OS-native notifications
- [x] Session restore (tabs, panel state)
- [x] LSP integration (diagnostics — rust-analyzer / tsserver / pyright / gopls)
- [x] Plugin system (commands, on-save hooks, themes, snippets, .zvplug distribution)
- [x] Phone remote (view/edit/command agents from a LAN browser)
- [x] VS Code-grade scrolling (fixed gutter, scrollBeyondLastLine, PageUp/PageDown)
- [x] 3 permission modes (🛡 Approve / ⚡ Full-auto / 🤖 Agent-first) + bulk switch for running sessions
- [x] Pet upgrades (4 looks, custom images, sizes, sleep/walk, sounds, approve/deny from the bubble)
- [ ] LSP completion & hover UI (foundation implemented; UI to come)
- [ ] Plugin grammars (TextMate) & registry sharing
- [ ] Inline diff view
- [ ] Split editor

## License
Apache License 2.0 — see [LICENSE](LICENSE) for details.

---

<div align="center">

**The agents are already fast enough.**<br>
**The next thing to get faster is you — the one in command.**

</div>
