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

## 🆕 Latest release: v0.7.1

- Auto-YES now types a digit into choice prompts only after the screen has been completely frozen for 30 seconds
- Normal scans never send digits, so bullet lists in agent output can no longer trigger a burst of keystrokes
- When a digit is sent, the notification states exactly which entry was picked

**Quality:** 2,567 tests, clean `cargo fmt --check`.

[v0.7.1 details](https://github.com/tacyan/zaivern-code/releases/latest) · [Previous releases](https://github.com/tacyan/zaivern-code/releases)

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

For plugin development, see the [plugin guide](docs/plugins.md) and [specification](docs/PLUGIN_SPEC.md).

## Contributing

Bug reports, feature requests, and pull requests are welcome. Check [Issues](https://github.com/tacyan/zaivern-code/issues) before opening a new report.

## License

[Apache License 2.0](LICENSE)

---

<div align="center">

**The agents are already fast. Now it is your turn to command faster.**

</div>
