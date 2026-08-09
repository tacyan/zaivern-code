<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# ⚡ Zaivern Code

**Run Claude Code, Codex, Gemini CLI, and other AI coding tools together — from one screen.**<br>
A Rust-native AI development cockpit for macOS, Windows, and Linux.

[日本語](README.md) | [**English**](README.en.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

[🌐 **Website**](https://zaivern.com/) · [⬇️ **Download**](https://github.com/tacyan/zaivern-code/releases/latest) · [🗒️ **Release history**](https://github.com/tacyan/zaivern-code/releases)

<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Zaivern Code running Claude Code, Codex, Gemini CLI, and other coding agents in parallel" />
</a>

</div>

## New to Zaivern Code?

Zaivern Code is not an AI model. It is an app that **brings your AI coding tools into one place**. Install and sign in to at least one tool, such as Claude Code, Codex, or Gemini CLI. You do not need all three.

Getting started takes three steps:

1. Install and sign in to an AI coding tool
2. Install Zaivern Code with the command below
3. Run `zai .` inside your project folder

## Stop keeping your AI agents waiting

Claude Code implements. Codex tests. Gemini CLI writes the docs. Zaivern Code brings scattered terminals into **one cockpit**.

| Before | With Zaivern Code |
|---|---|
| Jump between tabs for every agent | Monitor and control every agent from one screen |
| Paste the same instruction repeatedly | Broadcast one instruction to the whole fleet |
| Miss approvals and stalled sessions | Get live status, notifications, and one-click approval |
| Stay chained to your desk | Check progress, send instructions, and approve from your phone |

## 🚀 Install Zaivern Code

**macOS / Linux**

```bash
curl -fsSL https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.sh | sh
zai .
```

**Windows PowerShell**

```powershell
irm https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.ps1 | iex
zai .
```

The installer automatically downloads the right app for your operating system. Run the same command again whenever you want to update.

### Update

```bash
zai update            # Check for a newer release, show the command, then update
zai update --check    # Only check; nothing is executed
zai update --yes      # Update without asking for confirmation
```

The update method follows where the binary lives: `cargo install --force` when it sits in `~/.cargo/bin`, otherwise the one-liner installer above.

### Uninstall

```bash
zai uninstall --dry-run       # List everything that would be removed, with sizes (removes nothing)
zai uninstall                 # Show the list, then ask for `y` before removing
zai uninstall --keep-config   # Keep your settings (config.toml / state.toml)
zai uninstall --yes           # Remove without asking for confirmation
```

Only the **executable itself and `~/.zaivern`** (settings, session records, terminal logs) are removed. The OS app registration is unregistered at the same time. Any other `zai` still on your `PATH` is listed rather than deleted, so nothing outside those two locations is ever touched.

### After the first launch

1. Follow the two-minute guided tour
2. Click `+ Agent`
3. Choose an AI tool you already installed
4. Enter a task and send it

Start with one agent. Add a second or third after you are comfortable with the workflow.

## What you can do

### 🎛 Agent Cockpit

Arrange multiple AI tools in a grid and see at a glance whether each one is working or waiting. Zaivern includes launch presets for 29 tools, including Claude Code, Codex, and Gemini CLI.

### 📣 Broadcast

Send one instruction to every active AI at once, or select a single agent when you want focused control.

### 🛡 Approvals and supervision

Get notified when an AI asks for permission, stops responding, or exits unexpectedly. Automatic approval is **off by default** for safety.

### 📋 Fleet management

See whether each AI is thinking, editing, running, or checking its work. Once you are comfortable, you can give the same task to several agents and compare their results.

### 📱 Phone Remote

Check progress, send instructions, approve actions, and edit files from your phone. The easiest setup works over the same Wi-Fi network.

### 📝 Built-in code editor

Read code and review changes made by your AI tools without leaving the app. The editor can also open images, PDFs, CSVs, Markdown, and large files.

## 🆕 New in v0.10.0

**⿴ Multibuffer** — read search results, problems, and uncommitted changes as **one
surface with real surrounding context**, instead of opening files one by one. Click a
header to fold, click a line to jump there. Reachable from the "⿴ Open all" button in
the search and problems panels.

**⏸ Bulk send to stalled agents only** — "Everyone" interrupts agents that are still
working. This targets only the ones that have **stopped making progress** (idle,
stalled, looping, erroring). Agents waiting on an approval prompt are excluded — answer
those in the approval UI instead.

**Fixed multi-second freezes in large repositories** — fetching the branch name and the
gutter diff marks was blocking the UI thread. Worst frame: **4376ms → 20.8ms**.

## 🆕 Previous release: v0.8.0

**22 gaps in day-to-day usability, closed.** We studied superset, VS Code, cmux, orca and Zed,
and worked through the missing pieces starting with the ones that made the editor unusable.

**Made it a real editor**

- Commit, push, pull, hunk-level staging and history, all inside the app (you previously had to drop to a terminal)
- `.gitignore` is respected, and index truncation is now visible (`node_modules` used to swallow the index and silently break ⌘P)
- Unsaved buffers survive a restart (hot exit). If the file changed on disk, you get a diff instead of a silent overwrite
- Multi-cursor actually edits (⌘D only *selected* before)
- A real undo history, so formatting and code actions rewind in one ⌘Z
- Drag-and-drop moves with overwrite confirmation, trash instead of hard delete, and ⌘Z in the file tree

**VS Code parity**

Regex / case / whole-word / find-previous / highlight-all in buffer search · inline diagnostic squiggles and hover ·
matching-bracket highlight and rainbow brackets · vertical rulers · indentation auto-detect ·
MRU tab switching, pinning and preview tabs · recently-used ordering, `:123` and `@` symbols in Quick Open ·
two-stroke chords (⌘K ⌘S) and a keybinding editor · a settings UI ·
a workspace-wide problems panel · clickable file paths and URLs in the terminal

**As an AI cockpit**

- **Follow mode** — the editor tracks whatever the running agent is editing
- **Unread cursor** — jump to whichever agent is waiting on you
- **Rebuilt state detection** — structured output and hooks instead of guessing from the screen, and the source of each verdict is shown
- **Isolated worktrees and conflict detection** — if two agents touch the same file, you hear about it now, not at review time
- **Focused diff review** — a "2 / 5" counter and `]f` / `[f` to move between files
- ⌃1–⌃9 preset launch, automatic session naming, token usage and estimated cost

**Quality:** 3,134 tests, green CI on macOS, Linux and Windows, clean `cargo fmt --check`.

[v0.8.0 details](https://github.com/tacyan/zaivern-code/releases/latest) · [Previous releases](https://github.com/tacyan/zaivern-code/releases)

## Supported environments

| Item | Support |
|---|---|
| OS | macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64 |
| AI CLIs | 29 presets, including Claude Code, Codex, and Gemini CLI |
| Rust | 1.88+ when building from source |
| License | Apache-2.0 |

## Safety by default

- Approval-required mode is the default; Auto-YES requires explicit opt-in
- Privilege escalation always requires manual approval
- MCP environment-variable values are never displayed, only whether they are configured
- When using an SSH tunnel, the remote server binds to `127.0.0.1` only
- Child processes are stopped when sessions are destroyed or the app exits

## Frequently asked questions

### Are the AI tools included?

No. Install and sign in to the tools you want to use, such as Claude Code, Codex, or Gemini CLI.

### Do I need all three tools?

No. Zaivern Code works with a single AI tool. Starting with the tool you already use is the easiest path.

### Is Zaivern Code free?

Zaivern Code is free, open-source software under the Apache-2.0 license. Subscriptions and usage fees for each AI service are separate.

### Will it run commands without asking me?

Approval is required by default. Automatic approval only runs after you explicitly turn it on.

## Build from source

```bash
git clone https://github.com/tacyan/zaivern-code.git
cd zaivern-code
rustup update stable
cargo run --release -- .
```

### Tests

```bash
cargo fmt --all --check
cargo nextest run --profile ci
```

### Cross-OS checks (runnable locally, even from macOS)

Code behind `#[cfg(windows)]` or Linux-only branches never compiles in a macOS build.
These scripts let you catch such breakage without waiting for CI.

```bash
tools/linux-test.sh              # Reproduce the Linux tests in Docker
tools/windows-check.sh           # Type-check for Windows (MSVC)
tools/windows-check.sh --build   # Produce a real zai.exe (verifies linking)
```

The Windows side needs `cargo install cargo-xwin --locked` once.
Neither script touches the host `target/` directory.

For plugin development, see the [plugin guide](docs/plugins.md) and [specification](docs/PLUGIN_SPEC.md).

## Contributing

Bug reports, feature requests, and pull requests are welcome. Check [Issues](https://github.com/tacyan/zaivern-code/issues) before opening a new report.

## License

[Apache License 2.0](LICENSE)

---

<div align="center">

**The agents are already fast. Now it is your turn to command faster.**

</div>
