<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

**One cockpit for Claude Code, Codex, Gemini CLI, and the other AI coding CLIs you already use.**<br>
Launch, watch, and steer them from a single native app on macOS, Windows, and Linux.

[English](README.md) | [日本語](README.ja.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

[**Download**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**Quick Start**](#quick-start) ·
[**Documentation**](#documentation) ·
[**Website**](https://zaivern.com/)

<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Zaivern Code running Claude Code, Codex, Gemini CLI, and other coding agents side by side" />
</a>

If Zaivern Code looks useful to you, a ⭐ **Star** helps its development.

</div>

## Why Zaivern Code

Starting several AI coding CLIs is easy. Keeping track of them is not. Every agent
lives in its own terminal tab, asks for approval at its own pace, and edits files
without knowing what the others are doing.

| Without a cockpit | With Zaivern Code |
|---|---|
| Cycle through tabs to find who needs you | Every agent on one screen, with live status |
| Paste the same instruction into each tool | Broadcast once to the fleet, or target one agent |
| Miss an approval prompt and lose the run | Notifications and one-click approval |
| Stay at your desk while agents work | Check progress and approve from your phone |
| More parallel agents, more merge conflicts | A shared ledger keeps agents off each other's lines |
| A heavy editor competes with your agents for the machine | A single native binary, with damage-driven redraws |

That last row is a design constraint, not a slogan. Zaivern Code ships as one native
binary — no bundled browser engine, no Node runtime — and it redraws on damage instead
of running a permanent animation loop, which is what makes holding many PTYs at once
affordable in memory and latency. Idle cost is treated as a number rather than an
impression: `tools/idle-cpu.sh` measures it on your own machine, uses a plain `sleep`
process as the floor, and reports the raw CPU-time increment instead of a pass/fail
line. See [docs/idle-cost.md](docs/idle-cost.md) for what the measurement does and does
not tell you.

Zaivern Code is not an AI model and does not bundle one. It drives the CLIs you have
already installed and signed in to — one is enough to start.

## Quick Start

**Prerequisites.** Install and sign in to at least one supported AI coding CLI.
Zaivern Code ships launch presets for 33 of them, including Claude Code, Codex, and
Gemini CLI. You do not need more than one.

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

Rather not pipe a script into your shell? Download the archive for your platform from
[Releases](https://github.com/tacyan/zaivern-code/releases/latest), unpack it, and put
`zai` (or `zai.exe`) somewhere on your `PATH`. Then run `zai .` in a project folder.

Once the window is open:

1. Click `+ Agent` and pick a CLI you already have installed.
2. Type a task into the input box and send it.
3. Add a second agent once the first one feels comfortable.

`zai update` upgrades in place (`--check` only looks, `--yes` skips the prompt), and
`zai uninstall` removes it (`--dry-run` lists what would go). Uninstalling touches only
the executable and `~/.zaivern`; anything else on your `PATH` is listed, never deleted.

## Key Features

### Agent Cockpit

Tile several AI CLIs side by side and see at a glance which one is thinking, editing,
running, or waiting on you. Launch presets for 33 tools are built in, so adding an
agent is a two-click operation rather than a remembered command line.

### Broadcast

Send one instruction to every running agent from a single input box, or pick one agent
when you want focused control. Useful when the same correction applies to the whole
fleet.

### Status, Approvals, and Notifications

Zaivern Code surfaces permission prompts, stalls, and unexpected exits as notifications
you can act on in one click. Automatic approval is off by default and has to be turned
on deliberately.

### Phone Remote

Check progress, send instructions, approve actions, and edit files from your phone.
The simplest setup works over the same Wi-Fi network, and an SSH tunnel covers the
case where you are not on it.

### Conflict Coordination

Agents claim the files — or the individual line ranges — they are about to edit in a
shared ledger, and git hooks refuse a write that would collide. The section below
spells out what this covers and what it does not.

### Built-in Editor

Read code and review what your agents changed without leaving the app, including
images, PDFs, CSVs, and Markdown. Unsaved buffers survive a crash: the next launch
restores them, and if the file changed on disk in the meantime you are shown the
difference instead of being silently overwritten.

## Conflict Coordination

Running agents in parallel is cheap. Reconciling their output at review time is not.
Zaivern Code keeps a per-repository ledger of who owns which files and line ranges, and
installs git hooks and a merge driver so that a colliding write is stopped at the point
it happens rather than discovered during a merge.

```console
$ zai czero init      # ledger, git hooks, merge driver, .gitattributes — then self-diagnose
$ zai czero verify    # create a real conflict in a throwaway repo and prove it is stopped
```

`verify` does not merely read your configuration. It builds a disposable repository,
provokes an actual conflict, and reports whether each layer stopped it — your own
repository is never modified. `zai czero doctor` explains each layer with a fix, and
`zai czero uninstall` removes only what was added.

**What this prevents.** Git merge conflicts between agents that share the same ledger
and whose line regions stay safely apart.

**What this does not prevent.**

- **Semantic conflicts.** One agent changes a function signature while another keeps
  calling the old one. The regions never overlap, the merge is clean, and the code is
  still broken. These are surfaced, not blocked.
- **Interleaved edits in repetitive content.** Disjoint line regions guarantee
  *ownership*, not a clean merge. When surrounding lines repeat, git can align a hunk
  somewhere else and conflict anyway.
- **Repositories the hooks cannot reach.** Non-git folders accept a claim but enforce
  nothing; submodule interiors, bare repositories, and read-only checkouts are outside
  the four layers. `zai czero doctor` reports which case you are in.

Measurements, failure conditions, and the full list of limits live in
[docs/conflict-zero.md](docs/conflict-zero.md). The write guard is deliberately
**fail-open**: when `zai` is missing or the ledger is unreadable, commits go through.
Only a real, detected conflict is allowed to stop you.

## Supported Platforms

| Item | Support |
|---|---|
| OS | macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64 |
| AI CLIs | 33 launch presets, including Claude Code, Codex, and Gemini CLI |
| Rust | 1.88+ — only when building from source |
| License | Apache-2.0 |

A common setup is Claude Code implementing, Codex testing, and Gemini CLI writing docs,
but nothing in Zaivern Code assumes that split. Any combination works, including a
single agent.

## Safety

- Approval-required mode is the default; Auto-YES is opt-in per session.
- Privilege escalation always requires manual approval.
- MCP environment-variable values are never displayed — only whether they are set.
- Child processes are stopped when a session is destroyed or the app exits, so no
  orphaned agent keeps running in the background.

## Documentation

| Document | What it covers |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | What "conflict-free" claims, what it does not, and the measurements behind it |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | Which guarantees hold for which repository shape |
| [docs/anyrepo-proof.md](docs/anyrepo-proof.md) | Reproducing the experiment on your own repository |
| [docs/xplat-bench.md](docs/xplat-bench.md) | macOS and Linux results, measured side by side |
| [docs/idle-cost.md](docs/idle-cost.md) | How idle CPU cost is measured, and the current numbers |
| [docs/region-cost.md](docs/region-cost.md) | Cost of the line-region check itself |
| [docs/guard-edges.md](docs/guard-edges.md) | Where the write guard leaks, and how it is closed |
| [docs/bench-honesty.md](docs/bench-honesty.md) | Rules that keep the benchmarks from lying quietly |
| [docs/workspace-key.md](docs/workspace-key.md) | How per-workspace storage locations are derived |
| [docs/plugins.md](docs/plugins.md) · [docs/PLUGIN_SPEC.md](docs/PLUGIN_SPEC.md) | Writing plugins |

Release notes for each version are on the
[Releases page](https://github.com/tacyan/zaivern-code/releases).

## Contributing

Bug reports, feature requests, and pull requests are welcome. Please check
[Issues](https://github.com/tacyan/zaivern-code/issues) for an existing report before
opening a new one, and open a
[Pull Request](https://github.com/tacyan/zaivern-code/pulls) against `main`.

**Build from source**

```bash
git clone https://github.com/tacyan/zaivern-code.git
cd zaivern-code
rustup update stable
cargo run --release -- .
```

**Verify a change**

```bash
tools/verify.sh --lint           # format, compile, tests, and clippy in one pass
cargo nextest run --profile ci   # the full suite, the way CI runs it
```

Code behind `#[cfg(windows)]` or Linux-only branches never compiles in a macOS build,
so both are reproducible locally instead of waiting on CI:

```bash
tools/linux-test.sh              # run the Linux tests in Docker
tools/windows-check.sh           # type-check for Windows (MSVC)
tools/windows-check.sh --build   # produce a real zai.exe, verifying the link step
```

The Windows side needs `cargo install cargo-xwin --locked` once. Neither script writes
to the host `target/` directory.

## License

[Apache License 2.0](LICENSE)

---

<div align="center">

**The agents are already fast. Now it is your turn to command faster.**

</div>
