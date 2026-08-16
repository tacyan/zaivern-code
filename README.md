<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

**One cockpit for Claude Code, Codex, Gemini CLI, and the other AI coding CLIs you already use.**<br>
Launch, watch, and steer them from a single native app on macOS, Windows, and Linux.

**English** | [日本語](README.ja.md) | [简体中文](README.zh-CN.md) | [한국어](README.ko.md) | [Português (Brasil)](README.pt-BR.md) | [Español](README.es.md)

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

<!-- 出典: docs/conflict-zero.md §3.12 — zaivern-code / 書き手 16 / zai 0.14.0:
     素の git 26 ファイル・28 ハンク、zaivern あり 0/0・96/96 成立・拒否 0・30 件ずらし -->
**16 agents writing this repository in parallel** — plain git: **26 conflicted files / 28 hunks**.<br>
With the lease ledger: **0 / 0**, and all **96 edits landed** — none refused, 30 of them shifted to a free line range.<br>
[See the measurements →](docs/conflict-zero.md)

If Zaivern Code looks useful to you, a ⭐ **Star** helps its development.

</div>

## Why Zaivern Code

Starting several AI coding CLIs is easy. Keeping track of them is not. Every agent
lives in its own terminal tab, asks for approval at its own pace, and edits files
without knowing what the others are doing.

<!-- 出典: docs/conflict-zero.md §3.3 — 書き手 64 / 重なり 0.5:
     ベースラインは 57/64 のマージが衝突し 132 ハンク、ガード側は全規模で 0 ハンク -->

| Without a cockpit | With Zaivern Code |
|---|---|
| More parallel agents, more merge conflicts | A shared ledger keeps agents off each other's lines — 0 conflict hunks with 64 agents, where plain git produced 132 |
| Cycle through tabs to find who needs you | Every agent on one screen, with live status |
| Paste the same instruction into each tool | Broadcast once to the fleet, or target one agent |
| Miss an approval prompt and lose the run | Notifications and one-click approval |
| Stay at your desk while agents work | Check progress and approve from your phone |

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

Both installers verify the release archive against the published `checksums.txt`
**before unpacking it**, and abort without extracting or running anything if the
SHA-256 does not match — or if the checksums cannot be fetched at all.

Rather not pipe a script into your shell? Download the archive for your platform from
[Releases](https://github.com/tacyan/zaivern-code/releases/latest), unpack it, and put
`zai` (or `zai.exe`) somewhere on your `PATH`. Then run `zai .` in a project folder.
See [SECURITY.md](SECURITY.md) for how to verify the download by hand, check the
build provenance, or read the SBOM.

Once the window is open:

1. Click `+ Agent` and pick a CLI you already have installed.
2. Type a task into the input box and send it.
3. Add a second agent once the first one feels comfortable.

### Updating

```bash
zai update            # check for a newer release, show the command, then upgrade
zai update --check    # only look; changes nothing
zai update --yes      # upgrade without the confirmation prompt
```

`zai update` works whether or not the editor is running, and upgrades in place through
the installer script for your platform. Re-running the one-liner above does the same
thing.

`zai uninstall` removes it (`--dry-run` lists what would go). Uninstalling touches only
the executable and `~/.zaivern`; anything else on your `PATH` is listed, never deleted.

## Key Features

### Conflict Coordination (the reason this exists)

Agents claim the files — or the individual line ranges — they are about to edit in a
shared, per-repository ledger, and git hooks refuse a write that would collide.

<!-- 出典: docs/conflict-zero.md §3.8.1 — --layout disjoint / 64 体:
     B (ファイル単位の所有) 完了 1・拒否 63、Cref (行域) 完了 64・拒否 0・ハンク 0 -->
Line ranges are what makes this usable at scale. Point 64 agents at a single file and
a file-level lease lets exactly **1** of them through while refusing the other **63**;
with line-region ownership all **64** get through, nothing is refused, and the merge
still produces **0** conflict hunks.

<!-- 出典: docs/conflict-zero.md §3.12.2 — 錨の誤マッチによる二重配布と、その修正 -->
A region is tracked by an anchor — the contents of its first and last line — rather
than by a line number, so it survives edits made above it. If re-resolving that anchor
lands somewhere other than what the ledger recorded, the reading is discarded instead
of trusted, so a claim never silently migrates to another part of the file.

None of this catches a semantic conflict; the [section below](#conflict-coordination)
spells out what is covered and what is not.

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
The simplest setup works over the same Wi-Fi network. When you are not on it, either
of two transports takes over: **[Tailscale](https://tailscale.com/)**, if both machines
are already on the same tailnet, or an SSH tunnel through a host you can already reach.
Switching transport only changes where the server listens — the token, the port and the
page stay the same, so a QR already scanned on the phone keeps working.

Tailscale mode needs no bastion and no port forwarding: install
[Tailscale](https://tailscale.com/download) on the PC and the phone, sign both into the
same tailnet, then hit **🔒 Listen on Tailscale** in the phone-remote window. It binds
the tailnet address and `127.0.0.1` and nothing else, so the café or airport Wi-Fi you
happen to be on cannot see the port at all. Zaivern finds the tailnet address from the
kernel routing table and never shells out to the `tailscale` command — on macOS that CLI
is a shell wrapper that can hang forever when the daemon is not reachable, and a hung
child would freeze the UI.

### Built-in Editor

Read code and review what your agents changed without leaving the app, including
images, PDFs, CSVs, and Markdown. Unsaved buffers survive a crash: the next launch
restores them, and if the file changed on disk in the meantime you are shown the
difference instead of being silently overwritten.

## Conflict Coordination

A claim in the ledger is not advice: the hook refuses the colliding write at the
moment it is attempted, so the clash surfaces there instead of at merge time.

<!-- 出典: docs/conflict-zero.md §3.16.6 — dup_lines=0 は常に成立 (内容に依存しない)、
     conflict_files=0 は条件付き (帯 + 壁 + 昇順。反復的な内容では断ることがある) -->
Two guarantees hold to different degrees, and mixing them would overstate the case.
"No two agents are handed the same lines" is a property of the ledger and holds
regardless of what the files contain. "The merge then goes through in one pass" is
conditional: it needs a safety band, a unique line between the two regions, and
ascending order. Repetitive content can break the second while the first still holds,
and the gate refuses in that case rather than guessing.

What it cannot catch is a semantic conflict: one agent changes a function signature
while another keeps calling the old one, in a different file, with a perfectly clean
merge.

```console
$ zai czero init      # install the ledger, git hooks, and merge driver, then self-diagnose
$ zai czero verify    # create a real conflict in a throwaway repo and check that it stops
```

Scope, limits, and the measurements behind them are in
[docs/conflict-zero.md](docs/conflict-zero.md).

## Resource Use

<!-- 出典: docs/idle-cost.md §7 — 2026-08-15、同一マシン・同一セッションで
     Zed 1.15.0 / zai 0.16.0 / zai 0.17.0 を交互に 3 ラウンド、9/9 VALID。
     0.16.0 を陽性対照に入れてあるので「測定が生きていること」まで示せる。
     0.17.0 は測定床に張り付いているので必ず「≤」で書くこと -->

An editor you leave open all day should cost nothing while you are not typing.
Measured on one machine in a single session, alternating between apps three times
(macOS 26.5.2, on AC, 180-second observation windows, a neutral 4-file workspace):

| | Zed 1.15.0 | Zaivern Code 0.17.0 |
|---|---:|---:|
| Idle CPU (median of 3) | 0.761% of one core | **≤0.006%** — at the measurement floor |
| Download | 424.6 MB (`.app`) | **28.7 MB** (one binary) |
| RSS | 162.2 MB | 170.3 MB |

Two things this table is careful about:

- **`≤0.006%` is a floor, not a reading.** `ps` resolves CPU time to 1/100 s, so a
  180-second window cannot distinguish anything below 0.006%. All three rounds landed
  on exactly one tick. The honest claim is "at least 127x lower than Zed", not a ratio.
- **RSS is not a win, and we do not claim one.** Zed is also written in Rust; the two
  are within 5% of each other, which is noise. The number that differs by an order of
  magnitude is download size.

The same run measured Zaivern Code **0.16.0** at 8.933% — that is the positive control.
Without a version that produces a high reading in the same session, a near-zero result
cannot be told apart from a broken measurement. Idle cost dropped in 0.17.0 because the
guided tour no longer reserves frames unconditionally, and the two-second housekeeping
repaint is gone.

Reproduce it with `tools/idle-duel.sh --vs Zed --out /tmp/duel.tsv`. The harness refuses
to measure when it cannot measure honestly: it verifies the app is frontmost by pid,
requires the machine to be untouched, and records the evidence in every row. Full method,
raw numbers, and the traps we hit are in [docs/idle-cost.md](docs/idle-cost.md).

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

## FAQ

**How is this different from tmux with split panes?**

tmux tiles terminals; it has no idea what is running inside them. Zaivern Code reads
each agent's state, so it can show which one is thinking, editing, or blocked on an
approval prompt, and turn that prompt into a notification you answer in one click.
The part tmux has no equivalent for is the shared ledger: two agents cannot
physically write to the same lines, because a git hook refuses the second write at
the moment it is attempted rather than leaving it to be found at merge time.

**Does the lease ledger slow things down?**

<!-- 出典: docs/conflict-zero.md §1「意味しないこと」4 / §3.3 (掃引: 4〜8 体 p50 40〜50ms、
     64 体 p50 160ms、busy-deny 32 体 4 件・64 体 14 件) / §3.4 (ゲート 1536 回で p50 298.7ms)。
     体数だけでは決まらないので、必ず担当表の大きさを添えること -->
Yes, and it gets worse with scale, because the gate sits on the write path. On the
standard sweep — N writers over N×6 files — gate latency is p50 40–50 ms at 4–8
agents and p50 160 ms at 64. Agent count is not the only variable: a heavier
assignment table that calls the gate 1536 times reaches p50 298.7 ms at the same 64
agents, so any single "at 64 agents it costs X" number is incomplete without the
size of the workload. From 32 agents up the gate also starts answering `busy-deny`
when it cannot decide in time: it refuses rather than guessing, and a retry goes
through, but you see it as an occasional rejection. At one or two agents the gate is
not on your critical path.

**What does "zero conflicts" actually mean?**

Something narrower than it sounds, deliberately:

<!-- 出典: docs/conflict-zero.md §3.2 (書き手 8 / 重なり 1.00: 10/48 成立・38 件をゲートが停止)、
     §3.8.1 (disjoint / 64 体: 素の git のハンクは全規模 0。B は完了 1、Cref は 64)、§3.16.6 -->
- **Zero is bought by refusing writes.** With eight writers all aiming at the same
  files, 10 of 48 planned edits were written and the other 38 were stopped at the
  gate. The conflict count is 0; the throughput is not.
- **Line ranges that are far enough apart never needed help.** Plain git already
  merges those at zero conflicts. Line-region ownership is not doing something git
  cannot — it gives back the parallelism a file-level lease destroys (1 of 64 agents
  through, versus 64 of 64).
- **The two guarantees are not equally strong.** "No two agents get the same lines"
  always holds; "the merge goes through in one pass" is conditional and can fail on
  repetitive content.

[docs/conflict-zero.md](docs/conflict-zero.md) opens with exactly this boundary and
carries every measurement behind it, including the claims that were later refuted.

## Documentation

| Document | What it covers |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | What "conflict-free" claims, what it does not, and the measurements behind it |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | Which guarantees hold for which repository shape |
| [docs/plugins.md](docs/plugins.md) | Writing plugins, with the [format specification](docs/PLUGIN_SPEC.md) |
| [docs/README.md](docs/README.md) | Index of every other document, grouped by the claim it backs |

Release notes for each version are on the
[Releases page](https://github.com/tacyan/zaivern-code/releases).

## Contributing

Bug reports, feature requests, and pull requests are welcome. Please check
[Issues](https://github.com/tacyan/zaivern-code/issues) for an existing report before
opening a new one, and open a
[Pull Request](https://github.com/tacyan/zaivern-code/pulls) against `main`.

```bash
git clone https://github.com/tacyan/zaivern-code.git
cd zaivern-code
rustup update stable
cargo run --release -- .
```

[CONTRIBUTING.md](CONTRIBUTING.md) covers the rest: how to verify a change, how to run
the Linux and Windows checks locally, and the conventions this repository follows.

## License

[Apache License 2.0](LICENSE)

---

<div align="center">

**The agents are already fast. Now it is your turn to command faster.**

</div>
