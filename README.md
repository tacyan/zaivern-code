<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

### 64 AI Agents. One Repository. Zero Merge Conflicts.

**The coordination layer for parallel coding agents.**

Run Claude Code, Codex, Gemini CLI, and other coding agents on the same
repository — without merge-conflict chaos.

**English** | [日本語](README.ja.md) | [简体中文](README.zh-CN.md) | [한국어](README.ko.md) | [Português (Brasil)](README.pt-BR.md) | [Español](README.es.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

<!-- TODO: Replace with a 15-20 sec benchmark demo:
     64 agents on one repository / plain git 132 conflict hunks / Zaivern 0.
     The GIF below shows the cockpit, not the coordination result. -->
<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Zaivern Code running Claude Code, Codex, Gemini CLI, and other coding agents side by side" />
</a>

| 64 writers · same repository · same workload | Plain git | Zaivern Code |
|---|---:|---:|
| Merges that conflicted | 57 of 64 | **0** |
| Conflict hunks | 132 | **0** |

[See the methodology, trade-offs, and limitations →](docs/conflict-zero.md)

[**Quick Start**](#quick-start) ·
[**Benchmarks**](#benchmarks) ·
[**Docs**](#documentation) ·
[**Download**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**Website**](https://zaivern.com/)

</div>

## The problem

Running one coding agent is easy. Running four is not. Two agents editing the same
file is already enough:

- They edit the same lines, and you find out at merge time.
- You cannot see which agent is working, blocked, or quietly stuck.
- An approval prompt scrolls past in a tab you were not looking at.
- Integration becomes your job — every time.

The agents are not the bottleneck. The coordination between them is.

## The solution

Zaivern Code coordinates which parts of a repository each coding agent may safely edit.
Instead of discovering collisions at merge time, it catches overlapping work **before
the conflicting write lands** — and gives you one place to watch, steer, and recover
the agents you have running.

```text
Without Zaivern                          With Zaivern

Agent 1  ─┐                              Agent 1  ─┐
Agent 2  ─┤                              Agent 2  ─┤   ┌─────────────┐
Agent 3  ─┼─→ same files ─→ merge        Agent 3  ─┼─→ │ line-range  │ ─→ clean
   ...   ─┤                conflicts        ...   ─┤   │   ledger    │    integration
Agent 64 ─┘                              Agent 64 ─┘   └─────────────┘

132 conflict hunks                       0 conflict hunks
```

**You do not need 64 agents for this to matter.** Two agents editing the same file are
enough. Start with 2, scale to 64.

## Quick Start

Install and sign in to at least one supported AI coding CLI first — Zaivern Code ships
**33** launch presets, and one is enough to start.

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

In the window: click `+ Agent`, pick a CLI you have installed, and send it a task.

Enable conflict coordination for a repository:

```bash
zai czero init      # install the ledger, git hooks, and merge driver, then self-diagnose
zai czero verify    # create a real conflict in a throwaway repo and check that it stops
```

The installers verify the archive against the published `checksums.txt` **before
unpacking**, and abort if it does not match.
[Manual download, checksum verification, provenance, and SBOM →](SECURITY.md)

### Updating

```bash
zai update            # check for a newer release, show the command, then upgrade
zai update --check    # only look; changes nothing
zai update --yes      # upgrade without the confirmation prompt
```

Works whether or not the editor is running. `zai uninstall` removes it.

## Core features

### 1. Run agents in parallel without merge-conflict chaos

Agents claim files or line ranges before editing. If another live agent already owns
that region, a git hook refuses the colliding write — at write time, not at merge time.

In the 64-agent disjoint-range benchmark, all **64** agents landed their edits with
**0** conflict hunks, where a file-level lease would have let exactly 1 through.
[How line-range coordination works →](docs/conflict-zero.md)

### 2. Parallel agent management

Tile several AI CLIs side by side and see at a glance which one is thinking, editing,
running, or waiting on you. Adding an agent is two clicks, not a remembered command line.

### 3. Agent health and stall detection

Zaivern watches semantic progress, not pixels: an agent that stops making progress is
reported as **stalled**, and unexpected exits surface as notifications.

### 4. Broadcast

Send one instruction to every running agent from a single input box, or target one agent
when you want focused control.

### 5. Approvals

Approval-required mode is the default. Auto-YES is opt-in per session, privilege
escalation always needs a human, and MCP environment-variable values are never displayed.

### 6. Phone remote

Check progress, send instructions, approve actions, and edit files from your phone.
Use the same Wi-Fi, [Tailscale](https://tailscale.com/), or an SSH tunnel.

### 7. Built-in editor

Review code and agent changes without leaving Zaivern, including Markdown, images, PDFs,
and CSVs. Unsaved buffers are recovered after a crash.

Also included: plugins, and a UI available in six languages.
[Plugin docs](docs/plugins.md) · [Translation docs](docs/translating.md)

## How it works

1. **Launch** coding agents from one window, or attach ones you already run.
2. **Claim** files or line ranges before editing, anchored to the content around them.
3. **Guard** — a git hook refuses an overlapping write before it reaches merge time.
4. **Integrate** — non-overlapping changes merge through git as usual.

[Technical details →](docs/conflict-zero.md) ·
[which guarantees hold for which repository shape →](docs/czero-repo-shapes.md)

## Supported agents

Claude Code · Codex · Gemini CLI · Cursor Agent · GitHub Copilot CLI ·
**28 more** — 33 launch presets in total, plus 6 agents drivable over ACP.

Zaivern Code is not an AI model and does not bundle one: it drives the CLIs you have
already installed and signed in to. Any combination works, including a single agent.
Missing yours? [Request an integration](https://github.com/tacyan/zaivern-code/issues).

## Why Zaivern

|  | Terminal multiplexer | Generic agent dashboard | Zaivern Code |
|---|:---:|:---:|:---:|
| Run several agents at once | ✅ | ✅ | ✅ |
| One screen for all of them | ❌ | ✅ | ✅ |
| Knows agent state (thinking / blocked / stalled) | ❌ | varies | ✅ |
| Line-range ownership + write-time refusal | ❌ | ❌ | ✅ |
| Approvals as notifications | ❌ | varies | ✅ |
| Phone / remote control | ❌ | varies | ✅ |
| Single native binary, no runtime | varies | varies | ✅ |

## Benchmarks

**64 agents, one repository, same workload** (files = writers × 6, 50% file overlap):

| | Plain git | Zaivern Code |
|---|---:|---:|
| Merges that conflicted | 57 of 64 | **0** |
| Conflict hunks | 132 | **0** |

Zero is bought by refusing writes: 202 of 384 planned edits landed, the rest were
stopped at the gate. Where the line ranges are actually disjoint, all 64 agents land
and nothing is refused.

**This repository, 16 agents in parallel** (zai 0.14.0): plain git produced **26
conflicted files / 28 hunks**. With the ledger: **0 / 0**, and all **96 edits landed** —
none refused, 30 of them shifted to a free line range.

### What "zero conflicts" means

- Zaivern may **refuse** an overlapping write rather than let it become a merge conflict.
  The conflict count is 0; the throughput is not.
- It prevents overlapping line ownership. It does **not** detect semantic conflicts —
  one agent changing a signature while another keeps calling the old one merges cleanly.
- Line ranges far enough apart never needed help: plain git already merges those at zero
  conflicts. Line-range ownership gives back the parallelism a file-level lease destroys.

[Full methodology, per-scale numbers, gate latency, and limitations →](docs/conflict-zero.md)

## Supported platforms

| Item | Support |
|---|---|
| OS | macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64 |
| AI CLIs | 33 launch presets, plus 6 over ACP |
| Tests | 4,985, run on macOS, Linux, and Windows in CI |
| License | Apache-2.0 |

## Documentation

| Document | What it covers |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | What "conflict-free" claims, what it does not, and every measurement behind it |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | Which guarantees hold for which repository shape |
| [docs/plugins.md](docs/plugins.md) | Writing plugins, with the [format specification](docs/PLUGIN_SPEC.md) |
| [docs/README.md](docs/README.md) | Index of every other document, grouped by the claim it backs |

[Idle CPU and binary-size measurements →](docs/idle-cost.md) ·
[Release notes](https://github.com/tacyan/zaivern-code/releases)

## Try it

If parallel coding agents are part of your workflow, run Zaivern Code on your next
multi-agent task — `zai czero init` in the repository, then start two agents on the same
file and watch the second write get refused instead of merged badly.

## Community

- Found a coordination edge case? [Open an issue](https://github.com/tacyan/zaivern-code/issues).
- Using a coding agent that is not supported yet? [Request an integration](https://github.com/tacyan/zaivern-code/issues).
- Running 8, 16, 32, or 64 agents? Share your benchmark — `tools/conflict-bench.sh` and
  `tools/anyrepo-prove.sh` produce numbers comparable to the tables above.
- Built something with Zaivern Code? Show us your setup.

Pull requests are welcome against `main` — [CONTRIBUTING.md](CONTRIBUTING.md) covers
building from source (Rust 1.88+), verifying a change, and running the Linux and Windows
checks locally.

If Zaivern Code is useful to you, a ⭐ **Star** helps other people find it.

## License

[Apache License 2.0](LICENSE)
