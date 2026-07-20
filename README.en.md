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

Once installed, just type `zai .` inside any project folder — that's your cockpit (`zai [workspace path]` also works). **Run the same one-liner again at any time to update to the latest version.** Prebuilt binaries for every OS are also available directly from [**Releases**](https://github.com/tacyan/zaivern-code/releases/latest) (macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64).

---

## The view from the cockpit

### 🎛 See the whole fleet — Agent Cockpit

Click **🎛 Cockpit** in the toolbar (or press ⌘⇧C) and every running agent lines up in a grid. Each cell is not a decorative preview — it's a **live terminal you can type into directly**. Claude Code pushing the implementation forward, Codex repairing tests, Gemini CLI writing docs — the first time you watch five agents working at once on a single screen, expect goosebumps.

### 📣 One order, everyone moves — Broadcast

"Make sure the tests pass." "That approach is fine — continue." One input box, sent to every active session at the same instant. The nights of hopping between tabs pasting the same sentence are over as of today.

### 🛡 The reins stay in your hands — 3 permission modes

When you attack, use **⚡ Full-auto**: bypass flags are added to each CLI automatically, and the interactive prompts that survive even bypass mode (first-run warnings, folder-trust confirmations, plan approvals) are detected from the screen text and answered automatically — a two-layer system. When you defend, use **🛡 Approve**: any bypass flags smuggled into a command are stripped automatically, failing safe. And **👾 Agent-first** respects whatever flags your preset says, verbatim. Switch with one click in the toolbar; push the change to already-running sessions in bulk.

**Speed and safety were never an either/or.**

### 🔔 When you're needed, you will know — notifications and your sidekick 🐾 Zaigani

The instant an agent asks for approval — a popup appears, a sound plays, the session's ● turns yellow, and **Zaigani**, the little desktop pet strolling in the corner of your screen, starts fidgeting with an "❗ approval needed" sign. A bubble floats above its head: **✔ Approve / ✖ Deny**, one click. When a run succeeds it jumps 🎉; when one fails it goes 💥 with X-eyes.

You're not staring at cold logs — **a companion taps you on the shoulder.** Development where no agent ever waits on you feels better than you'd imagine.

### 📱 Leave your desk — the command continues — Phone Remote

Tap 📱 in the top bar, scan the QR code, and any phone on the same Wi-Fi becomes your remote control. From the sofa, from the balcony, while the coffee brews — approvals, new instructions, file edits, progress checks. **While the agents are working, there is no reason left for you to be chained to a desk.** (Per-launch random token auth, LAN only.)

### 🎤 Just speak — one mic button, nothing else

**Press 🎤. That's it.** Everything you say flows into your agent's input box and keeps flowing — no key to hold down, no shortcut to memorize, no browser window on the side. It runs until you press the **⏹ right next to it**.

The important part is what happens next: **nothing**. Speech recognition makes mistakes, so Zaivern never presses Enter for you. Read what landed in the box, fix it if you want, and send it when you're satisfied. **Speak fast, send deliberately.**

And when Enter clears the box, **the mic is still listening**. The next thought can start the moment the last one is sent — your rhythm never breaks.

- Send to **🎯 the active agent** or **📣 every agent** — switchable while still recording
- Pick "active" and the destination follows you as you move between tabs
- Set a spoken trigger word (e.g. "send") and only then will Enter be sent for you — off by default, so sending stays manual
- Language and engine live in the ▾ menu next to 🎤 in the top bar

And it works **on every platform**. `voice_engine = "auto"` picks the route for you, so pressing 🎤 is still the whole interaction.

| Where you are | What actually runs |
|---|---|
| **macOS** | The system's built-in recognizer. Fully offline. |
| **Windows** | Windows' own speech recognition, offline — but **only if a recognizer for your language is installed**. The Japanese one ships solely with Japanese-language Windows, and Microsoft deprecated the whole feature in Win11 24H2. Zaivern probes for it at runtime and quietly falls back to the browser route below if it isn't there. |
| **Linux / Windows without a recognizer** | A local voice page (`http://127.0.0.1:<port>/voice`) opens and you speak into **the browser's microphone**. Chrome/Chromium is preferred, and Zaivern always tells you which browser it opened — Edge's `webkitSpeechRecognition` can't be trusted. |
| **Phone (remote)** | **Your phone keyboard's own dictation** — the 🎤 on Gboard, or iOS voice input. |

The phone is the odd one out for a reason: the remote is plain HTTP over your LAN, and browser speech recognition flatly requires a secure context. It used to fail there in a silent retry loop. Now the page notices and points you at keyboard dictation instead — **no HTTPS, no page permission, nothing to grant**. Browsers with no Speech API at all (iOS Safari, Firefox) get the same guidance. The text still only reaches the input box. **Enter is still yours to press.**

Want your own recognizer? Set `voice_command` as before — on anything but macOS, it always wins.

### 📝 And the final stroke is still yours — a Zed-inspired editor

Even in an era where AI writes 90% of the code, the last 10% — the architectural judgment calls, the naming, the one line you take responsibility for — belongs to a human. So Zaivern keeps a sharp pen right beside the commander's chair: syntect syntax highlighting, LSP diagnostics, Git diff gutters, a fuzzy palette (⌘P), VS Code-grade file operations and scrolling. **The moment you feel like writing, you can.**

---

## Why Rust — a heavy cockpit is no cockpit at all

- **No Electron. No Node.** A single native binary with GPU rendering via egui. Instant startup; idle memory lighter than one browser tab
- **A real PTY terminal** (portable-pty + vt100). Claude Code's full-screen TUI runs as-is. 256-color / TrueColor, bracketed paste, scrollback — plus the unglamorous compatibility work: terminal queries (device attributes, cursor position, background colour) are actually *answered*, because a query left hanging either freezes the TUI waiting for a reply or dumps raw escape text into its input box. Cursor-shape changes, focus in/out reporting, and OSC 52 clipboard writes are handled too
- **One codebase for macOS / Windows / Linux.** Child processes are killed automatically on exit — no orphan processes left behind
- Lineage: **Zed's speed × Cmux's parallel agents × AGI Cockpit's pilot-seat UX**

---

## Feature reference

Everything below is the manual for each instrument on the flight deck.

### 📝 Editor
- Syntax highlighting via syntect (Rust / TS / Python / Go / Markdown and many more, auto-detected by extension)
- Tabs, line-number gutter, unsaved indicator (●), save confirmation before closing
- VS Code-grade file operations in the file tree: ➕ new file / 📂 new folder (inline input), ✏ rename (open tabs' paths and languages follow automatically), 🗑 delete (with confirmation dialog)
- Right-click menu: open / new / rename / delete / "Send path to agent (@path)" / copy full path
- In-file search (⌘F, hit count, jump-to-hit centered on screen)
- VS Code-grade scrolling: fixed gutter, scrollBeyondLastLine, PageUp/PageDown
- Fuzzy command palette (⌘P for files, ⌘⇧P for commands)
- Git branch display, automatic Japanese UI font fallback

### 👾 Multi-agent
- Launch agent presets with one click (⌘⇧A) and run multiple sessions in parallel
- Per-session status (●/○), uptime, restart, force-kill
- **29 CLI agents are recognized by a built-in catalog**: Claude Code / Codex / Grok / Cursor / GitHub Copilot / OpenCode / MiMo Code / Amp / OpenClaude / Antigravity / Pi / oh-my-pi / Hermes / Devin / Goose / Auggie / Autohand / Crush / Cline / Command Code / Continue / Droid / Kilo Code / Kimi / Kiro / Mistral Vibe / Qwen Code / Rovo Dev / Aider
- **`Agent +` opens the catalog picker** — a searchable list you add from. Agents already installed on your machine sort to the top; the ones that aren't show you the install command instead of failing silently
- Permission modes (🛡 Approve / ⚡ Full-auto) auto-apply to every agent in the catalog. **You never have to write the flags in your preset** — and for Goose and Aider, which have no blanket auto-approve flag at all, the same mode is applied through environment variables instead
- A CLI agent that isn't in the catalog still runs in parallel — just register it as a preset
- Push permission-mode changes to running sessions via each row's 🛡 button (or "🛡 switch all")

### 🔔 Notifications + sounds
- Approval-wait, success (✅), and failure (❌ + exit code) announced via popup + OS-native sounds (can be turned off)
- When the window is unfocused, notifications also go to macOS Notification Center (Linux: notify-send)

### 🐾 Desktop pet "Zaigani"
- Blinks, follows your cursor with its eyes, wanders around; dozes off when idle → deep sleep (💤), startled hop when you come back
- Agent-linked reactions: marching "⚙ n" while agents run (faster with more agents), grooving (🎵) at 3+, fidgeting on approval-wait, 🎉 on success / 💥 on failure
- 💬 Approval bubble: ✔ Approve / ✖ Deny / Open with one click (keys sent to the PTY are customizable via `pet_approve_keys` / `pet_deny_keys`)
- Click to toggle the Cockpit (jumps to the waiting session if one exists), drag to reposition (auto-saved)
- 🎭 4 looks (blocky / crab / cat / cloud) + swap in any image you like, 📏 3 sizes

### 📦 Bundled plugins (working from the moment you install)
Every major capability ships as a plugin. On first launch they unpack into `~/.zaivern/plugins/` and are **enabled as-is**. There is nothing to configure.

| Plugin | What it does |
|---|---|
| 🌳 `worktrees` | Split work trees and run them in parallel. **Hand one instruction to several agents at once**, compare the results, merge the one you like |
| ⚖️ `agent-compare` | Line up the parallel results side by side, compare how much each one changed, pick the winner and take it |
| 💬 `diff-review` | Collect comments on diff lines, then send them back to the agent in one go |
| 📋 `tasks` | List issues and change requests, and spin a working branch straight out of one. Diffs and comment posting included |
| 🖧 `remote-host` | Run, sync, and launch agents on another machine |
| 🎯 `element-capture` | Pick an element on screen and pass its structure, styles, and a cropped image into the prompt |
| 📊 `usage-meter` | Show agent usage in a panel |
| ⚡ `quick-actions` | Detect the project type and run test / build / format immediately |

They are just shell scripts. **Read them, copy them, rewrite them.** Anything you don't want can be disabled from the 🔌 tab.

### 🔌 Plugins (build one, share it, get one)
If you can write shell, you can write one. A single folder under `~/.zaivern/plugins/<name>/` plus a `plugin.toml`. No Rust, no rebuild.

- **▶ Commands**: run a shell command and feed the result back into the editor
  - `input` = `none` | `selection` | `file`
  - `output` = `replace` | `insert` | `new_tab` | `notify` | `silent` | `agent_prompt` | `panel` | `actions`
  - Scope by language with `langs = ["rust"]`, bind a shortcut with `keybind = "cmd+alt+f"`, run automatically on save with `on_save = true` (formatter-friendly)
  - Runs in the background with a timeout. If you edited the buffer mid-run, it will not overwrite you
- **📊 Panels**: add your own display area to the sidebar (refresh manually, on open, or on an interval; Markdown rendering supported)
- **🪝 Hooks**: fire on startup, file open/close, save, **agent completion**, approval-wait, git changes, or a fixed interval
- **⚙️ Settings**: declare values for the user to fill in; they arrive as `ZV_CFG_<KEY>`
- **🎨 Themes / ✂️ Snippets**: bundle the usual editor-compatible formats unchanged

**Drive the app itself**: set `output = "actions"` and every line of stdout becomes an instruction (JSON Lines).

```sh
echo '{"action":"open_file","path":"src/main.rs","line":42}'
echo '{"action":"agent_prompt","agent":"claude","text":"write tests for this function"}'
```

Open files, notify, open tabs, run things in the terminal, rewrite a panel, **talk to an agent** — all fair game. `agent_prompt` **only places the text in the input box** unless you explicitly pass `submit`, so nothing runs off on its own.

Three buttons to manage it all: **➕ New** (generates a full sample template), **📤 Export** (writes a `.zvplug` you can hand to anyone), **📦 Install** (just pick a received `.zvplug` / `.zip`).

```toml
# plugin.toml example: auto-format JSON on save
[plugin]
name = "json-fmt"
version = "0.1.0"
description = "Format JSON on save"
api = 2

[[command]]
title = "Format JSON"
run = "python3 -m json.tool"
input = "file"
output = "replace"
langs = ["json"]
on_save = true
keybind = "cmd+alt+f"
```

📖 **The full guide lives in [docs/plugins.md](docs/plugins.md)** — a 3-minute build walkthrough, every field, an action cheat sheet, and what to do when it misbehaves.

### ⌨️ Drive it from the command line
`zai` can control a running editor from the outside. Plugins can use it — and so can **the agents themselves**.

```bash
zai open src/main.rs --line 42     # open a file
zai notify "build is green"        # raise a notification
zai prompt "write tests for this"  # drop text into the agent's input box (does not send)
zai run "cargo test"               # run it in the terminal
zai status "deploying"             # show it in the status bar
zai plugin list                    # list plugins
zai plugin new <name>              # scaffold one
```

Bare `zai` and `zai .` still launch the GUI, exactly as before.

### 🔤 Language servers (LSP)
If `rust-analyzer` / `typescript-language-server` / `pyright-langserver` / `gopls` is on your PATH it starts automatically and shows diagnostics (errors/warnings). The line-number gutter turns red/yellow, and the status bar shows `⛔count ⚠count`. Editing works normally even without any server installed.

Setup examples: `rustup component add rust-analyzer` / `npm i -g typescript-language-server typescript` / `npm i -g pyright` / `go install golang.org/x/tools/gopls@latest`

### ⌨️ Japanese input (IME)
Type Japanese directly inside the terminal. Uncommitted composition text is overlaid with an underline at the cursor position, and only committed text is sent to the agent.

### 🌿 Git line gutter
In a git repository, line numbers are color-coded by diff (green = added, yellow = modified). The status bar shows the branch name + changed-file count (±N).

### 📚 Multi-folder workspace
Open several folders at once. List them as arguments — `zai frontend backend shared` — or add one later from the command palette's "Add folder to workspace".

- The file tree lists each root under its own heading (with a single root, it looks exactly as it always did)
- File search spans every root. **Only when the same relative path exists in more than one root** does Zaivern prefix the folder name to tell them apart — no noise the rest of the time
- git detects the real repository per root (`rev-parse --show-toplevel`), so opening a *subdirectory* of a repo still shows correct diffs. Two roots inside the same repository share one git state
- Session restore is keyed on the *set* of roots, so reordering them still restores the same workspace

### 🐙 GitHub integration
List pull requests and issues, read a PR's diff, and switch branches — all through the `gh` command. **No extra auth setup**: if you've run `gh auth login`, it already works. On a machine without `gh`, the panel is disabled cleanly rather than erroring at you.

A PR diff opens as a read-only tab rendered in the inline diff view, with added and removed lines colour-coded.

### 🧭 Open in an external IDE
Send the file you're editing to another editor **with your cursor line intact**. VS Code / Cursor / Zed / Trae / Kiro / Sublime / the JetBrains family / Xcode / Fleet / Neovide / Emacs are supported.

Installed IDEs are detected automatically, and only those are offered. If your `code` command has been hijacked by some other product, Zaivern resolves the real binary and identifies it correctly. Apps that ship no CLI are handed the file over their URL scheme.

### 🔭 Agent supervision and hand-off
Run several agents and sooner or later one goes quiet, repeats the same failure, or dies. This is the layer that notices and does something about it.

- **Detection**: stalling (silence with no progress), looping (the same output over and over), a storm of errors, abnormal exit, an approval prompt left unattended, runaway output. Spinners and counters are correctly treated as *not* progress
- **Intervention escalates**: record → notify → auto-approve → nudge → restart → stop. **Restart and stop always ask you first**, by default, because in-flight work gets lost either way. In 🛡 Approve mode, nothing above "notify" is ever done automatically
- **Reassignment**: a stalled task is handed to a different agent. An agent that already failed a task never gets it back. **The hand-off does not happen until the previous holder is confirmed stopped** — otherwise two agents edit the same files. When the retry budget is spent, it escalates to you instead of looping forever
- **Inter-agent messaging**: a message is delivered only when the recipient is idle, because interrupting mid-generation corrupts its input. Hop limits, rate limits, and round-trip detection all apply, and any message that couldn't be delivered is recorded with the reason

**Hand out the work.** Create a "task" from the cockpit and either assign it to a specific agent or let auto-assignment decide. The list shows state, owner, and attempt count, and anything the system gave up on shows as a red `NeedsUser`. If an assignment is refused, you get the reason in plain words — nothing is worse than silence.

**Let the agents talk to each other.** Besides sending by hand, an agent can write this at the start of a line and it goes straight to the target:

```
[ZAI-TO:backend] the migration passed, go ahead on the API side
[ZAI-TO:ALL] moved the shared type definitions into types.ts
```

There is no LLM guessing at which sentences look like they're addressed to someone. **Only the line-start marker counts — deterministic, on purpose.** And when an injected message is echoed back onto the screen, it does not get re-sent as a new message (get that wrong and messages multiply without end).

### 💡 Super Agent — give the watchdog a brain
The supervision itself runs on Rust rules. You don't need an LLM to spot a stall or a loop, and **if the watchdog is an LLM, nobody is left to notice when the watchdog breaks.** So detection stays deterministic code.

On top of that, you can ask an AI the one question code is bad at: *"okay, so what's the right move here?"* In the cockpit's **💡 Super Agent**, you just **pick which CLI agent supervises**.

- The default is **"none"**. Pick nothing and no LLM is ever queried
- **Only agents that support non-interactive execution are selectable.** One that doesn't would open an interactive screen and never return, so it isn't offered in the first place
- It receives **only redacted recent output**. API keys, GitHub tokens, email addresses, and home-directory paths are masked automatically
- **The supervisor is itself supervised, like everyone else.** Exempt it and you've built a single point of failure
- **It can still do normal work.** You don't have to burn a slot on supervision alone. It just won't be asked to diagnose its own stall — asking a frozen agent why it froze gets you nothing
- **AI advice does not move the permission gate.** If it recommends a restart or a stop, you still confirm — even in ⚡ Full-auto mode
- If the response can't be parsed, **nothing happens**. A watchdog that manufactures actions out of ambiguous answers is more dangerous than one that stays quiet

### 💾 Session restore
On restart, the previous tabs, active tab, and panel state are restored automatically per workspace (`~/.zaivern/sessions/`).

### 📱 Phone Remote in detail
- **What you can do**: view/edit/save open files, switch tabs, search & open workspace files, view agent terminals, send instructions, approve (Enter / Esc / ^C / ↑ / ↓ / Tab / ⇧Tab / 1 / 2 / 3 / y buttons), and run commands (save, new file, Cockpit, font ±, approval-mode switch, and more)
- **How it works**: a tiny built-in HTTP server (port 8899, auto-fallback to 8900–8919 if busy). Pure `std::net` — zero extra crates
- **Security**: authenticated with a random token generated per launch (embedded in the QR URL). Tokenless API access gets a 401. LAN only

---

## Installing (manual)

The one-liners at the top are fastest. `install.sh` places a prebuilt binary from GitHub Releases at `~/.local/bin/zai`, and on platforms without a matching binary it builds from source with Rust (auto-installing rustup if needed). If Zaivern Code is already installed, the script acts as an **updater** — it fetches the latest version and also refreshes any stale `zai` binary left elsewhere on your PATH.

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

# Open several folders at once
./target/release/zai ~/dev/frontend ~/dev/backend ~/dev/shared

# Mix in a file argument and it opens as a tab
./target/release/zai ~/dev/my-project README.md
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

# Default permission mode (auto-applied to all 29 agents in the catalog)
#   "ask"   = user approval required every time (safe, default)
#   "auto"  = auto-YES to everything (bypass flags added per CLI)
#   "agent" = agent-first (use whatever flags the preset command says)
approval_mode = "ask"

# Desktop pet 🐾
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
icon = "👾"
command = "claude"

[[agents]]
name = "Claude Code (full-auto)"
icon = "⚡"
command = "claude --dangerously-skip-permissions"

[[agents]]
name = "Codex"
icon = "💡"
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
# icon = "💡"
# command = "claude --model claude-opus-4-8"
# env = { MAX_THINKING_TOKENS = "31999" }
```

- `command` runs through the login shell (`$SHELL -lc`), so your PATH and aliases just work.
- `env` injects preset-specific environment variables (model selection, API-key switching, etc.).
- `cwd = "~/some/dir"` pins the working directory (defaults to the workspace).
- **Per-project overrides**: drop a `.zaivern.toml` in the workspace root to set theme, fonts, approval mode, and extra agents per project.
- **Choices made in the UI are auto-saved to `~/.zaivern/state.toml`** (theme, approval mode, pet settings) — your handwritten config.toml stays clean. "Reload settings" gives config.toml priority.

### Command tricks
- Right-click in the file tree → "👾 Send path to agent" types `@path ` (Claude Code's file-reference syntax)
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
├── file_tree.rs     Lazy-loading file tree (multi-root) + context menu
├── fuzzy.rs         Fuzzy-match scoring
├── palette.rs       Command palette state & action definitions
├── keybinds.rs      Customizable keybindings
├── git.rs           git CLI integration (repo detection, branch, per-line diff marks)
├── git_panel.rs     Git side panel (list and switch branches / worktrees)
├── github.rs        GitHub integration (via gh CLI — PR/Issue/diffs, async)
├── diff.rs          Unified diff parser + inline diff view
├── ide.rs           Hand-off to external IDEs (open at the current line)
├── panels.rs        Rendering for the GitHub panel, PR diff tabs, IDE integration
├── supervisor.rs    Agent supervision (stall/loop/abnormal-exit detection, escalating intervention)
├── coordinator.rs   Inter-agent messaging and task reassignment
├── orchestration.rs Task creation UI, hand-off driving, message send/receive assembly
├── diagnostician.rs Supervising LLM (calls the chosen CLI agent non-interactively to diagnose)
├── markdown.rs      Markdown parsing and preview rendering
├── html.rs          HTML preview rendering
├── jsonc.rs         Reading JSON with comments (JSONC)
├── cli.rs           `zai` subcommands (the control channel for driving the app from outside)
├── lsp.rs           Minimal LSP client (stdio JSON-RPC, diagnostics)
├── terminal.rs      PTY sessions + vt100 rendering + approval-prompt detection/auto-reply
├── agents.rs        Session management (launch/restart/destroy/broadcast/permission modes)
├── remote.rs        Phone remote (built-in HTTP server, QR code, token auth)
├── voice.rs         Voice input (records until stopped, inserts without sending, auto-picks mac/Windows/browser)
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
- [x] 3 permission modes (🛡 Approve / ⚡ Full-auto / 👾 Agent-first) + bulk switch for running sessions
- [x] Pet upgrades (4 looks, custom images, sizes, sleep/walk, sounds, approve/deny from the bubble)
- [x] Voice input (🎤/⏹ only, records until stopped, inserts into the input box for a manual Enter, configurable destination/language/engine)
- [x] Cross-platform voice input (built-in on macOS; Windows' own recognizer when one is installed; a browser page on Linux and on Windows without one; keyboard dictation guidance on phones)
- [x] Inline diff view (unified diff parsing and colour-coded rendering)
- [x] Multi-folder workspace (open several folders at once)
- [x] GitHub integration (PR / Issue lists, PR diff viewing, branch operations)
- [x] Agent catalog (29 CLI agents configured automatically per permission mode)
- [x] External IDE integration (open in another editor with the cursor line intact)
- [x] Agent supervision (detect stalls, loops, and abnormal exits, then intervene in stages)
- [x] Inter-agent messaging and task reassignment
- [x] Terminal compatibility hardening (query responses, cursor shape, focus reporting, OSC 52)
- [x] Super Agent (pick the supervising LLM from the UI, redaction, destructive actions always confirmed)
- [x] Task creation from the cockpit and hand-off of stalled tasks
- [x] Inter-agent messages (sent with a `[ZAI-TO:target]` line-start marker)
- [ ] LSP completion & hover UI (foundation implemented; UI to come)
- [ ] Plugin grammars (TextMate) & registry sharing
- [ ] Split editor

## License
Apache License 2.0 — see [LICENSE](LICENSE) for details.

---

<div align="center">

**The agents are already fast enough.**<br>
**The next thing to get faster is you — the one in command.**

</div>
