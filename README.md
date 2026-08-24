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
![Rust](https://img.shields.io/badge/rust-1.88%2B-orange)

<!-- TODO: Replace with a 15-20 sec benchmark demo:
     64 agents on one repository / plain git 132 conflict hunks / Zaivern 0.
     The GIF below shows the cockpit, not the coordination result. -->
<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Zaivern Code running Claude Code, Codex, Gemini CLI, and other coding agents side by side" />
</a>

<!-- 出典: docs/conflict-zero.md §3.3 — 書き手 64 / 重なり 0.5 / ファイル数 = 書き手 × 6:
     ベースラインは 57/64 のマージが衝突し 132 ハンク、ガード側は全規模で 0 ハンク。
     ガードが書けたのは 202/384 (残りは拒否) なので、必ず添えること -->

| 64 writers · same repository · same workload | Plain git | Zaivern Code |
|---|---:|---:|
| Merges that conflicted | 57 of 64 | **0** |
| Conflict hunks | 132 | **0** |

Zero is bought by refusing writes: 202 of 384 planned edits landed, the rest were
stopped at the gate. Where the line ranges are actually disjoint, all 64 agents land
and nothing is refused. [Every number, and what it does not claim →](docs/conflict-zero.md)

[**Quick Start**](#quick-start) ·
[**Benchmarks**](#benchmarks) ·
[**Docs**](#documentation) ·
[**Download**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**Website**](https://zaivern.com/)

</div>

## The problem

Running one coding agent is easy. Running four is not.

Two agents editing the same file is already enough to hit it:

- They edit the same lines, and you find out at merge time.
- You cannot see which agent is working, blocked, or quietly stuck.
- An approval prompt scrolls past in a tab you were not looking at.
- Integration becomes your job — every time.

The agents are not the bottleneck. The coordination between them is.

## The solution

Zaivern Code does not make an agent smarter. It is the layer **between** agents and
the repository: a per-repository ledger of who owns which lines, a git hook that
refuses a colliding write at the moment it happens, and one screen that shows what
every agent is doing.

```text
Without Zaivern                          With Zaivern

Agent 1  ─┐                              Agent 1  ─┐
Agent 2  ─┤                              Agent 2  ─┤   ┌─────────────┐
Agent 3  ─┼─→ same files ─→ merge        Agent 3  ─┼─→ │ line-range  │ ─→ clean
   ...   ─┤                conflicts        ...   ─┤   │   ledger    │    integration
Agent 64 ─┘                              Agent 64 ─┘   └─────────────┘

132 conflict hunks                       0 conflict hunks
```

The clash surfaces at write time, not at merge time. That is the whole idea.

**You do not need 64 agents for this to matter.** Two agents editing the same file
are enough. Start with 2, scale to 64.

## Quick Start

**Prerequisites.** Install and sign in to at least one supported AI coding CLI.
Zaivern Code ships launch presets for **33** of them, including Claude Code, Codex,
and Gemini CLI. One is enough to start.

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

Then, in the window: click `+ Agent`, pick a CLI you have installed, and send it a
task. Add a second agent when the first one feels comfortable.

To turn on conflict coordination for a repository:

```console
$ zai czero init      # install the ledger, git hooks, and merge driver, then self-diagnose
$ zai czero verify    # create a real conflict in a throwaway repo and check that it stops
```

### Updating

```bash
zai update            # check for a newer release, show the command, then upgrade
zai update --check    # only look; changes nothing
zai update --yes      # upgrade without the confirmation prompt
```

`zai update` works whether or not the editor is running and upgrades in place through
the installer for your platform. Re-running the one-liner above does the same thing.
`zai uninstall` removes it (`--dry-run` lists what would go); it touches only the
executable and `~/.zaivern`.

Both installers verify the release archive against the published `checksums.txt`
**before unpacking it**, and abort without extracting anything if the SHA-256 does not
match — or if the checksums cannot be fetched at all. Prefer not to pipe a script into
your shell? Download the archive from
[Releases](https://github.com/tacyan/zaivern-code/releases/latest), unpack it, and put
`zai` on your `PATH`. [SECURITY.md](SECURITY.md) covers verifying the download by hand,
the build provenance, and the SBOM.

## Core features

### 1. Run agents in parallel without merge-conflict chaos

Agents claim the files — or the individual **line ranges** — they are about to edit in
a shared, per-repository ledger. A git hook refuses a write that would collide.

<!-- 出典: docs/conflict-zero.md §3.8.1 — --layout disjoint / 64 体:
     B (ファイル単位の所有) 完了 1・拒否 63、Cref (行域) 完了 64・拒否 0・ハンク 0 -->
Line ranges are what makes this usable at scale. Point 64 agents at one file and a
file-level lease lets exactly **1** through while refusing the other **63**. With
line-range ownership all **64** land, nothing is refused, and the merge still produces
**0** conflict hunks.

A region is tracked by an **anchor** — the contents of its first and last line — not by
a line number, so it survives edits made above it. If re-resolving that anchor lands
somewhere other than what the ledger recorded, the reading is discarded rather than
trusted, so a claim never silently migrates elsewhere in the file.

### 2. Parallel agent management

Tile several AI CLIs side by side and see at a glance which one is thinking, editing,
running, or waiting on you. Launch presets for 33 tools are built in, so adding an
agent is two clicks rather than a remembered command line.

### 3. Agent health and stall detection

Zaivern watches semantic progress, not pixels: an agent that has stopped making
progress is reported as **stalled**, and unexpected exits and permission prompts
surface as notifications you can act on in one click.

### 4. Broadcast

Send one instruction to every running agent from a single input box, or target one
agent when you want focused control. Useful when the same correction applies to the
whole fleet.

### 5. Approvals

Approval-required mode is the default. Auto-YES is opt-in per session, privilege
escalation always needs a human, and MCP environment-variable **values** are never
displayed — only whether they are set.

### 6. Phone remote

Check progress, send instructions, approve actions, and edit files from your phone.
Same Wi-Fi works out of the box. Off it, two transports take over:
**[Tailscale](https://tailscale.com/)** (binds the tailnet address and `127.0.0.1` and
nothing else — the café Wi-Fi cannot see the port) or an SSH tunnel through a host you
can already reach. Switching transport keeps the token, the port, and the page, so a QR
code already scanned on the phone keeps working.

### 7. Built-in editor

Read code and review what your agents changed without leaving the app — including
images, PDFs, CSVs, and Markdown. Unsaved buffers survive a crash: the next launch
restores them, and if the file changed on disk you are shown the difference instead of
being silently overwritten.

Also included: a plugin system ([spec](docs/PLUGIN_SPEC.md)), and a UI that ships in
six languages — English, 日本語, 简体中文, 한국어, Português (Brasil), Español —
switchable without a restart. Adding a language is one JSON file, no rebuild:
[docs/translating.md](docs/translating.md).

## How it works

1. **Launch** your coding agents from one window (or attach ones you already run).
2. **Claim** — before writing, an agent's edit is registered in the per-repository
   ledger as a file or a line range, anchored to the content of its first and last line.
3. **Guard** — a git hook refuses a write that would collide with a live claim. It
   refuses rather than guesses; a refused claim can be shifted to a nearby free range.
4. **Integrate** — because no two agents were ever handed the same lines, the merge is
   a normal merge.

Deeper: [docs/conflict-zero.md](docs/conflict-zero.md) ·
[docs/czero-repo-shapes.md](docs/czero-repo-shapes.md)

## Supported agents

Claude Code · Codex · Gemini CLI · Cursor Agent · GitHub Copilot CLI ·
**28 more** — 33 launch presets in total, plus 6 agents drivable over ACP.

Zaivern Code is not an AI model and does not bundle one. It drives the CLIs you have
already installed and signed in to. A common setup is Claude Code implementing, Codex
testing, and Gemini CLI writing docs, but nothing assumes that split — any combination
works, including a single agent.

Missing your agent? [Request an integration](https://github.com/tacyan/zaivern-code/issues).

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

"varies" means exactly that — the capabilities of other dashboards are not something
this table is in a position to measure.

## Benchmarks

<!-- 出典: docs/conflict-zero.md §3.3 (重なり 0.5)、§3.8.1 (disjoint)、§3.12 (実リポジトリ) -->

**Scaling, 50% file overlap** (files = writers × 6). Guard-side conflict hunks are 0 at
every scale:

| Writers | Plain git: conflicted merges / hunks | Zaivern: written / planned | Gate p50 |
|---:|---:|---:|---:|
| 4 | 2 / 4 · 2 hunks | 14 / 24 | 40 ms |
| 8 | 4 / 8 · 7 hunks | 27 / 48 | 49 ms |
| 16 | 11 / 16 · 19 hunks | 55 / 96 | 64 ms |
| 32 | 27 / 32 · 56 hunks | 106 / 192 | 113 ms |
| 64 | **57 / 64 · 132 hunks** | 202 / 384 | 160 ms |

**This repository, 16 agents in parallel** (zai 0.14.0): plain git produced **26
conflicted files / 28 hunks**. With the ledger: **0 / 0**, and all **96 edits landed** —
none refused, 30 of them shifted to a free line range.

### What "zero conflicts" does and does not mean

Deliberately narrower than it sounds:

- **Zero is bought by refusing writes.** With eight writers all aiming at the same
  files, 10 of 48 planned edits were written and 38 were stopped at the gate. The
  conflict count is 0; the throughput is not.
- **Line ranges far enough apart never needed help.** Plain git already merges those at
  zero conflicts. Line-range ownership is not doing something git cannot — it gives back
  the parallelism a file-level lease destroys (1 of 64 agents through, versus 64 of 64).
- **The two guarantees are not equally strong.** "No two agents are handed the same
  lines" holds regardless of file contents. "The merge then goes through in one pass" is
  conditional: it needs a safety band, a unique line between the two regions, and
  ascending order — repetitive content can break the second while the first still holds.
- **Semantic conflicts are out of scope.** One agent changes a signature while another
  keeps calling the old one, in a different file, with a perfectly clean merge.
- **The gate is on the write path.** From 32 agents up it answers `busy-deny` when it
  cannot decide in time — it refuses rather than guesses, and a retry goes through. At
  one or two agents it is not on your critical path.

[docs/conflict-zero.md](docs/conflict-zero.md) opens with exactly this boundary and
carries every measurement behind it, including the claims that were later refuted.

### Idle cost

<!-- 出典: docs/idle-cost.md §7 — 2026-08-15、同一マシン・同一セッションで
     Zed 1.15.0 / zai 0.16.0 / zai 0.17.0 を交互に 3 ラウンド、9/9 VALID。
     0.17.0 は測定床に張り付いているので必ず「≤」で書くこと -->
An editor you leave open all day should cost nothing while you are not typing.
Measured on one machine in a single session, alternating between apps three times
(macOS 26.5.2, on AC, 180-second windows, neutral 4-file workspace):

| | Zed 1.15.0 | Zaivern Code 0.17.0 |
|---|---:|---:|
| Idle CPU (median of 3) | 0.761% of one core | **≤0.006%** — at the measurement floor |
| Download | 424.6 MB (`.app`) | **28.7 MB** (one binary) |
| RSS | 162.2 MB | 170.3 MB |

`≤0.006%` is a floor, not a reading: `ps` resolves CPU time to 1/100 s, so a 180-second
window cannot distinguish anything below it. The honest claim is "at least 127x lower",
not a ratio. RSS is a tie and we do not claim otherwise. Method, raw numbers, and the
positive control are in [docs/idle-cost.md](docs/idle-cost.md).

## Supported platforms

| Item | Support |
|---|---|
| OS | macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64 |
| AI CLIs | 33 launch presets, plus 6 over ACP |
| Tests | 4,985, run on macOS, Linux, and Windows in CI |
| Rust | 1.88+ — only when building from source |
| License | Apache-2.0 |

## Documentation

| Document | What it covers |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | What "conflict-free" claims, what it does not, and every measurement behind it |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | Which guarantees hold for which repository shape |
| [docs/idle-cost.md](docs/idle-cost.md) | Idle CPU method and raw numbers |
| [docs/plugins.md](docs/plugins.md) | Writing plugins, with the [format specification](docs/PLUGIN_SPEC.md) |
| [docs/translating.md](docs/translating.md) | Adding a language with one JSON file |
| [docs/README.md](docs/README.md) | Index of every other document, grouped by the claim it backs |

Release notes are on the
[Releases page](https://github.com/tacyan/zaivern-code/releases).

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

Pull requests are welcome against `main`:

```bash
git clone https://github.com/tacyan/zaivern-code.git
cd zaivern-code
rustup update stable
cargo run --release -- .
```

[CONTRIBUTING.md](CONTRIBUTING.md) covers how to verify a change and how to run the
Linux and Windows checks locally.

If Zaivern Code is useful to you, a ⭐ **Star** helps other people find it.

## License

[Apache License 2.0](LICENSE)
