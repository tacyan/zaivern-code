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
| **More agents means more merge conflicts** | **Same file, different lines — no conflict** |

The real cost of running agents in parallel is not wall-clock time; it is **resolving
conflicts at review time**. With 64 agents on one file, plain git produced
**48 conflicting branches, 960 conflict lines, and 48 manual fixes**. Zaivern Code
produced **zero conflicts and zero manual fixes — and all 64 agents landed their work**
([the numbers](docs/conflict-zero.md)).

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

### 🧩 Conflict-free — same file, different lines

**This is the core of the product.** Ownership is per *line*, so a large file like
`src/app.rs` can be shared by any number of agents. If your lines are taken, you are
handed nearby free lines instead of being refused.

```console
$ zai czero init      # install every layer at once, then self-diagnose
$ zai czero verify    # create a real conflict and prove it gets stopped
```

### 🎛 Agent Cockpit

Arrange multiple AI tools in a grid and see at a glance whether each one is working or waiting. Zaivern includes launch presets for 33 tools, including Claude Code, Codex, and Gemini CLI.

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

## 🆕 New in v0.14.0 — no conflicts, even in the same file

**🧩 Region ownership.** Until now the only way to protect parallel agents was
"nobody may write a file someone else holds." Safe, but **one agent holding a large
file locks everyone else out of it**. From v0.14.0 you hold *lines*:
`zai lease claim 'src/app.rs#L1200-1260'`.

Measured with 64 agents hammering a single 2000-line file
(`tools/coedit-bench.sh --agents 64 --lines 2000`):

| Protection | Landed | Refused | Conflicting branches | Conflict lines | Manual fixes |
|---|---:|---:|---:|---:|---:|
| None (plain git) | 64 | 0 | **48** | **960** | **48** |
| Per file (≤ v0.13) | **1** | 63 | 0 | 0 | 0 |
| Per line, no shifting | 11 | 53 | 0 | 0 | 0 |
| **Per line + negotiation (v0.14)** | **64** | **0** | **0** | **0** | **0** |

**Zero conflicts was already true in v0.13. What v0.14 buys is parallelism** —
1 of 64 agents could write; now all 64 can.

**🔀 Shift instead of refuse.** If the lines you asked for are taken, Zaivern hands
you nearby free lines automatically (`zai lease claim --shift`). The price is
**how far from your request you landed**: p50 129 lines, p95 253, max 281, and zero
allocations outside the file. Only requests that explicitly opt in are moved — a
region is tied to *the content that lives there*, so the default is never to move it.

**🤝 Agents recognise each other, Erlang-style.** Every participant has an identity
(`incarnation` is the start time, so **a recycled OS pid can never be mistaken for
you**) and a mailbox with per-sender FIFO delivery. `link` / `monitor` / `DOWN` with
`trap_exit`. **When an agent dies holding regions, its regions are released
automatically** — "let it crash", applied to editing. Measured release: 23.2 ms.

**🔒 Proof that a merge lands in one shot.** If the changed regions of N branches are
far enough apart, `git merge` **cannot** conflict. Verified exhaustively against real
git across 240 cases with **zero misses**. When the proof holds, N branches integrate
with zero human steps — without ever touching the working tree (`merge-tree` →
`commit-tree`, then one atomic ref update), so a failure leaves nothing half-merged.

**🧬 List appends stop conflicting, in any repository.** `.gitignore`, `CHANGELOG.md`,
`package.json` dependencies, `import` blocks — conflicts where the right answer is
"keep both lines" are resolved by **reading the content**, with no markers to add.
On a marker-free benchmark: **80% fewer conflict lines, zero wrong auto-resolutions**.
For files that are not lists, the result is **byte-for-byte identical to plain git**.

**🚦 One command in any repository.**

```console
$ zai czero init      # ledger, git hooks, merge driver, .gitattributes — then self-diagnose
$ zai czero verify    # actually create a conflict and prove it gets stopped
```

`verify` does not just read configuration. It **creates a real conflict in a
throwaway repository and proves it is stopped** (your repository is never touched),
so "installed but not working" cannot happen silently. `zai czero doctor` reports each
layer with a reason and a fix; `zai czero uninstall` removes only what was added.

The numbers — **including the conditions where this does not help** — are in
[docs/conflict-zero.md](docs/conflict-zero.md).

## Supported environments

| Item | Support |
|---|---|
| OS | macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64 |
| AI CLIs | 33 presets, including Claude Code, Codex, and Gemini CLI |
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

### Is "zero conflicts" really zero?

**With protection on, yes** — but here are the conditions, stated honestly. If two
regions are at least `SAFE_BAND` (3) lines apart, git's three-way merge **structurally
cannot** emit a conflict. With 64 agents on one file we measured zero conflict hunks
and zero manual fixes.

There are things this **cannot** do. Changes in *different* files that stop fitting
together (one side changes a signature, the other keeps calling the old way) are
impossible to prevent — those are detected and shown, not blocked. Numbers and limits:
[docs/conflict-zero.md](docs/conflict-zero.md).

### Does it work on an existing repository?

`zai czero init`. Existing git hooks (husky, lefthook, pre-commit framework) are kept
and called first, with their exit code respected. Hand-written `.gitattributes` lines
are preserved. `zai czero uninstall` removes only what was added.

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
