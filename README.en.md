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

**Windows** — PowerShell (same behavior: prebuilt binary, or builds from source — installing Rust via rustup if needed):

```powershell
irm https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.ps1 | iex
```

Once installed, just type `zai .` inside any project folder — that's your cockpit (`zai [workspace path]` also works). **Run the same one-liner again at any time to update to the latest version.** Prebuilt binaries for every OS are also available directly from [**Releases**](https://github.com/tacyan/zaivern-code/releases/latest) (macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64).

The installer also **registers "Zaivern Code" in your OS app list** — launch it without a terminal from Launchpad / Spotlight on macOS, the application menu on Linux, or the Start Menu on Windows (your home directory becomes the workspace). Register or remove manually with `zai app install` / `zai app uninstall`. It also shows up as **"Zaivern"** in the OS process list (Activity Monitor / `pgrep -i zaivern` on macOS, Task Manager on Windows, `ps` on Linux).

On first launch a **26-step guided tour** walks you through the cockpit, highlighting each instrument where it actually lives (about two minutes). Esc or Skip gets you out at any point, and "Restart tutorial" in the 💡 menu or the command palette brings it back.

---

## 🆕 v0.7.0 — the look, the languages, and the splits all line up

- **🔍 Two-storey zoom** — the **whole UI** (`⌘+` / `⌘-` / `⌘0`) and **per-file** text size, the same split VS Code makes: grow the interface, or grow only the code you are reading. The factor is saved with the session.
- **🎨 Colour themes went from 3 to 11** (7 dark / 4 light). The terminal and the editor draw from the same palette, so switching a theme moves everything together.
- **🔤 Syntax highlighting for the languages that were missing.** TypeScript, Swift, Kotlin, Dart, Zig, TOML, Dockerfile, Terraform and friends are absent from syntect's default set; they now come from a lightweight tokenizer (`grammar.rs`) where **one language is one TOML block**, plus a bundled plugin. It carries no colours of its own — tokens are **mapped onto theme scopes**, so changing the theme changes the code too.
- **🎛 The Cockpit no longer squashes past six tiles** — it scrolls vertically and every tile stays readable.
- **📋 The kanban stops crying wolf** — only a *sustained* anomaly is called "stalled / error". The designated super agent (the commander) is obvious from **its frame and crown**.
- **⊞ Splitting a terminal now behaves exactly like launching a new agent.** It used to inherit the parent pane's preset and working directory; splitting is now only about **where the pane goes**, and what starts in it is identical to `👾 Agent ＋` (the default preset, in the workspace folder).

**Quality**: **2565 tests, all passing**, `cargo fmt --check` clean, clippy ratchet (`-D warnings` plus the frozen debt list) with **0 warnings**.

## 🆕 v0.6.0 — no file you cannot open, no key that does nothing

- **Every file opens.** Anything unreadable as text lands in a **hex viewer**; video and audio become **info cards** (WAV and MP4 have their duration and resolution parsed in-house); ZIP / JAR / WHL list **their contents**. Routing is by **leading magic bytes** (40+ formats), not by extension — **a lying extension still opens correctly**.
- **The editor caught up on the rest of VS Code** — **⇹ splits** (`⌘\` / `⌥⌘\` / `⌘1`–`⌘3`; **the layout survives a restart**), a **🗺 minimap**, **🔗 breadcrumbs**, **💡 quick fix `⌘.`** and signature help `⇧⌘Space`, **👤 Git blame**, drag-to-reorder tabs, and running `.vscode/tasks.json`.
- **🔌 An MCP server manager** and **🧩 a skills / slash-command manager**, folding scattered config into one table. **MCP `env` values are never displayed** — key names and "is it set", nothing else.
- **📱 Your phone no longer has to be on the same Wi-Fi.** Open an **SSH reverse tunnel** to a host you already use (the phone needs no SSH client). **While the tunnel is up the server binds to `127.0.0.1` only**, closing the cleartext listener. No password is entered and none is stored.
- **🔁 Automatic account failover on rate limits** (off by default). What convinced it is shown as a rung on the ladder — **structured protocol > vendor hook > state file > screen** — and screen-derived evidence is **labelled an estimate**.
- **⌨️ Headless `zai`** — `zai worktree`, `zai session`, `zai agent`. All take `--json`; exit codes are **0 / 1 / 2** across the board.
- **A key that does nothing fails CI.** Every binding is pinned by a test that it reaches somewhere which actually consumes it, and **the shortcuts printed on screen are generated from the keybinding table** (rebind in `config.toml` and the labels follow). `⌃Space` (completion) is reserved by macOS for "previous input source", so the default moved to **`⌘I`** (⌃Space still works where it reaches the app).

**Quality**: CI **green on macOS, Windows and Linux**, **2488 tests**, **0 rustc warnings**. CI gained `cargo fmt --all --check` and a clippy ratchet that freezes the 26 existing debts and **only blocks new ones**.

## 🆕 v0.5.1 — nothing moves just because you launched it

- **The session list only shows conversations from the folder you have open** — the per-branch (worktree) grouping is gone. The list stays the same for as long as the folder does (the same scoping the VS Code Claude Code extension uses).
- **Launching starts nothing** — restoring last session's agent tabs is now off by default. The single way back into an earlier conversation is the 💬 sessions tab (`restore_agents = true` brings the old behaviour back).
- **The fleet board is full-window** — it used to be a tab inside the 300 px bottom panel; it is now a centre view on par with the Cockpit and the deck, so the 8 lanes are not squeezed into a third of the screen.

## 🆕 v0.5.0 — pick the shape of your command post

A large update after fifteen 0.4.x patches ([release notes](https://github.com/tacyan/zaivern-code/releases/latest)).

- **Vertical agent deck** (`⌘⇧L`) — running agents only, in a single column. Together with the Cockpit and the board, you can pick the shape that fits the moment. **Terminal splits** landed too.
- **The board now has 8 lanes** — thinking / editing / running / verifying are all visible, so you never have to guess whether an agent is working or stuck. The live pane goes fullscreen.
- **Branch switching** — switch from the toolbar; a branch held by another worktree, an in-progress merge, or uncommitted changes stop the switch with the reason spelled out (**no `git stash`** — the stash stack is shared across worktrees). The session list only ever covers the folder you have open, so moving between branches never changes what you see.
- **The editor reaches VS Code parity** — full LSP, multiple carets / block selection, folding / guides / sticky headers, regex and glob bulk replace, Emmet, images / PDF / CSV / huge files, and a Markdown preview that renders Mermaid diagrams and TeX math.
- **Paste images straight in** — `⌘V` / `Ctrl+V`, in every composer in the Cockpit and the deck.
- **Idle CPU 0.70% → 0.13%** (0.03% unfocused). Repaints are damage-driven only.

**Quality**: CI **green on macOS, Windows and Linux**, **all 2182 tests passing**, **0 build warnings and 0 errors**.
This release also cures a fatal Linux bug where `kill` could take down every one of your processes, the Windows Cockpit freeze, terminal tiles that stayed black, and the Japanese IME's confirm-Enter leaking through to the agent.

---

## The view from the cockpit

### 🎛 See the whole fleet — Agent Cockpit

Click **🎛 Cockpit** in the toolbar (or press ⌘⇧C) and every running agent lines up in a grid. Each cell is not a decorative preview — it's a **live terminal you can type into directly**. Claude Code pushing the implementation forward, Codex repairing tests, Gemini CLI writing docs — the first time you watch five agents working at once on a single screen, expect goosebumps.

And if you open a file while the Cockpit is up, the view splits — **editor on the left, Cockpit on the right** (drag the divider to resize; MD/HTML preview keeps working). Read the code without ever taking your eyes off the fleet.

### 📣 One order, everyone moves — Broadcast

"Make sure the tests pass." "That approach is fine — continue." One input box, sent to every active session at the same instant. The nights of hopping between tabs pasting the same sentence are over as of today.

### 🛡 The reins stay in your hands — 3 permission modes

When you attack, use **⚡ Full-auto**: bypass flags are added to each CLI automatically. When you defend, use **🛡 Approve**: any bypass flags smuggled into a command are stripped automatically, failing safe. And **👾 Agent-first** respects whatever flags your preset says, verbatim. Switch with one click in the toolbar; push the change to already-running sessions in bulk.

The interactive prompts that survive even bypass mode (first-run warnings, folder-trust confirmations, plan approvals) have their own switch: **"⚡ Auto-YES"** (in the 🐾 menu / `pet_auto_yes`). **It defaults to off — approvals always wait for you.** Turn it on and prompts are detected from the screen text and answered automatically, for every session no matter which mode it was launched in. And each prompt is answered **exactly once** — no misfires on a "(y/n)" quoted in the conversation log, no hammering Enter.

#### 💡 Configuring Auto-Approve Rules per Project or Globally
If you want to persist the Auto-Approve rule inside the AI agents (e.g., Antigravity, Claude Code) themselves, you can place rules files as follows:

- **Per Project** (`.gemini/rules.md` in the project root):
  ```bash
  mkdir -p .gemini && cat << 'EOF' > .gemini/rules.md
  # Auto-Approve Settings
  - Execute tools and commands autonomously with Auto-Approve without waiting for confirmation.
  - Always display git diffs when modifying files.
  EOF
  ```
- **Globally Across All Projects** (`~/.gemini/rules.md`):
  ```bash
  mkdir -p ~/.gemini && cat << 'EOF' > ~/.gemini/rules.md
  # Global Auto-Approve Settings
  - Proceed autonomously with Auto-Approve across all projects.
  - Always display git diffs when modifying files.
  EOF
  ```

**Speed and safety were never an either/or.**

### 🔔 When you're needed, you will know — notifications and your sidekick 🐾 Zaigani

The instant an agent asks for approval — a popup appears, a sound plays, the session's ● turns yellow, and **Zaigani**, the little desktop pet strolling in the corner of your screen, starts fidgeting with an "❗ approval needed" sign. A bubble floats above its head: **✔ Approve / ✖ Deny**, one click. When a run succeeds it jumps 🎉; when one fails it goes 💥 with X-eyes.

You're not staring at cold logs — **a companion taps you on the shoulder.** Development where no agent ever waits on you feels better than you'd imagine.

### 📱 Leave your desk — the command continues — Phone Remote

Tap 📱 in the top bar, scan the QR code, and any phone on the same Wi-Fi becomes your remote control. From the sofa, from the balcony, while the coffee brews — approvals, new instructions, file edits, progress checks. **While the agents are working, there is no reason left for you to be chained to a desk.** (Per-launch random token auth.)

**And it no longer has to be the same Wi-Fi.** The same 📱 panel offers "Remote connection (SSH)": relay through a host you can already SSH into and it opens a **reverse SSH tunnel**. The phone needs no SSH client — it just opens a URL. **While the tunnel is up the server binds to `127.0.0.1` only**, so the cleartext LAN listener stays closed. No password is entered and none is stored (authentication is left to the OS's `ssh` and your keys).

**On Windows this needs one permission, once.** Windows blocks all inbound connections by default, so without a rule your phone's connection is dropped by the OS — the PC is listening, the QR is correct, and yet nothing happens on the phone. Nothing on screen told you why, so the story used to end at "phone remote doesn't work on Windows". Now the 📱 panel checks the inbound rule itself and, if it is missing, says so and offers a **"🛡 Allow inbound" button** (one administrator prompt). It allows **only this executable on TCP 8899-8919**, and you can revoke it from the same panel. From a terminal: `zai firewall status` / `zai firewall allow` / `zai firewall revoke`.

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

Even in an era where AI writes 90% of the code, the last 10% — the architectural judgment calls, the naming, the one line you take responsibility for — belongs to a human. So Zaivern keeps a sharp pen right beside the commander's chair: syntect syntax highlighting, LSP diagnostics and quick fixes (⌘.), Git diff gutters and Git blame, a fuzzy palette (⌘P), a minimap and breadcrumbs, **editor splits (⌘\)**, VS Code-grade file operations and scrolling. **And there is no file it cannot open** — anything unreadable as text lands in the hex viewer, and video, audio and archives each open in the shape that suits them. **The moment you feel like writing, you can.**

---

## Why Rust — a heavy cockpit is no cockpit at all

- **No Electron. No Node.** A single native binary with GPU rendering via egui. Instant startup; idle memory lighter than one browser tab. **A 10 MB download, about 19 MB once installed** (measured on macOS arm64) — no runtime and no `node_modules` ride along
- **Watching costs no CPU.** Unconditional repainting is gone; each frame is requested only when the actual state needs one. Measured over a 30-second window on a release build: **0.13% focused, 0.03% unfocused**. Launch with `ZV_IDLE_TRACE=1` and it prints the real fps and the reason for each decision every second
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
- Auto-closing brackets & quotes (auto-pair on open, surround the selection, skip over closers, Backspace deletes an empty pair at once)
- VS Code-grade scrolling: fixed gutter, scrollBeyondLastLine, PageUp/PageDown
- Fuzzy command palette (⌘P for files, ⌘⇧P for commands, **`@` for agents, `#` for git worktrees** — one box searches files, commands, sessions, and worktrees)
- **Drag & drop**: drop a file-tree item, or any file/image from your OS, onto an agent's terminal and `@path` lands in its input field (nothing is submitted). Drop on the editor to open a tab; drop a folder to add it to the workspace
- **🖼 Image viewer**: `png` / `jpg` / `jpeg` / `gif` / `webp` / `ico` open as a tab. Fit-to-window by default; `−` / `＋` / `100%` buttons and Ctrl(⌘)+scroll take it from 0.05x to 32x. Transparency is drawn as a checkerboard, and the footer shows `width×height px · file size` (and says so explicitly when the image was downscaled to stay inside the GPU texture limit). Read-only. Animated GIFs show the first frame
- **📄 Open a PDF in the editor**: text is extracted with [pdf-extract](https://crates.io/crates/pdf-extract) and shown as a **read-only** tab with `── page i / N ──` separators. ⌘F find-in-file works on it as usual. Extraction runs on a worker thread: anything finishing within 250 ms appears immediately, slower files show `⏳ loading…` and fill in afterwards — the UI never blocks. PDFs over 32 MB are not extracted and say so
- **↩ Word wrap / · whitespace rendering**: toggle from the View menu or the command palette (spaces render as `·`, tabs as `→`). The starting value comes from `word_wrap` / `show_whitespace` in `config.toml` (both default `false`), overridable per project in `.zaivern.toml`
- **📐 Folding, indent guides, sticky headers, 🔖 bookmarks**: **⌥⌘[** toggles the fold at the cursor, **⌥⌘]** unfolds everything (fold-by-level lives in the command palette). Nesting guides and the parent headers pinned to the top of the viewport (up to 3, same as VS Code) are always drawn. **⌥⌘B** bookmarks a line and the palette jumps to the next / previous one; **⇧⌘T** reopens the tab you just closed
- **✏ Multiple carets / column selection**: **⌘D** selects the next occurrence and adds a caret. The palette has "add cursor above / below", "start → finish column selection", and "paste into every caret" — which undoes in a single step
- **✂️ Snippets and Emmet**: VS Code syntax (`$1`, `${1:default}`, `${TM_FILENAME}`, `${1|a,b|}` choices, mirrors) expands on Tab. Drop your own `*.json` / `*.code-snippets` (JSONC allowed) into `~/.zaivern/snippets/`. In HTML / JSX-family files, Emmet abbreviations like `ul>li*3` and `div.cls#id` expand too
- **🔤 Encodings and line endings**: "Reopen with encoding" / "Save with encoding" from the command palette. The list only offers encodings this build **actually round-trips**, measured at startup rather than assumed. Line endings convert to LF / CRLF / CR, and a mixed file reports the breakdown. "Trim trailing whitespace" and "Insert final newline" on save are View-menu toggles (off by default)
- **📊 CSV / TSV table view and large files**: `.csv` / `.tsv` render as a table via "Toggle table view". Past 4 MB, highlighting and folding switch off; past 50 MB the file opens read-only (the banner spells out which limits are in effect); files over 512 MB are not opened
- **👁 Markdown / HTML preview** (**⌘⇧V**): GFM tables, task lists, footnotes and autolinks, collapsed front matter, raw HTML tags, and local / `data:` images. **```mermaid fences render as diagrams** (`graph` / `flowchart` / `sequenceDiagram`; unsupported kinds say so), and `$…$` / `$$…$$` TeX math is typeset — **both in pure Rust, with no added dependencies**
- **🔢 Every file opens**: anything that is not readable as text **always lands in the hex viewer**. There is no "cannot open this file" dead end
  - **Hex dump**: the classic `offset | 16 hex bytes | ASCII` layout. **Only the visible rows are built**, so drawing costs the same no matter how long the file is. A tab holds up to 4 MB and says so explicitly when it stops there
  - **🎬 Video / 🎵 audio info cards**: `mp4` / `mov` / `mkv` / `webm` / `avi` / `m4v` and `mp3` / `wav` / `flac` / `aac` / `ogg` / `m4a`. **WAV and MP4 have their duration, resolution, sample rate and channel count parsed in-house** — no external command, no added crate. Fields the header does not give up are printed as `—` rather than guessed
  - **📦 ZIP / JAR / WHL contents**: stored name, original size, compressed size and timestamp, **parsed in-house with no new dependency** (nothing is extracted)
  - **Routed by content, not by name**: the leading magic bytes identify **more than 40 formats**, so **a lying extension still lands in the right viewer** (a PNG named `.txt` opens as an image, a ZIP named `.log` opens as an archive). The genuinely ambiguous pairs are disambiguated by reading further — `RIFF` splitting into WAV / AVI / WebP, and `CAFEBABE` shared between Mach-O universal binaries and Java class files
- **🗺 Minimap** (off by default): a zoomed-out view of the buffer down the right edge. Click to jump, drag to scroll, and **search hits, LSP diagnostics and bookmarks are overlaid as marks** so a problem far away is still visible at that scale. It costs 64 px of body width, hence the default-off, and it hides itself automatically on narrow windows. Toggle from the View menu, the palette's "Toggle minimap", or `minimap` in `config.toml`
- **🔗 Breadcrumbs** (on by default): `folder › folder › file › symbol` under the tab strip. **The path part does not need an LSP** — it is known the moment the file opens. Click a segment to navigate; when the row does not fit, **the middle is elided** and both ends are kept. Toggle from the View menu, the palette's "Toggle breadcrumbs", or `breadcrumbs` in `config.toml`
- **⇹ Editor splits** (**⌘\** right / **⌥⌘\** down / **⌘1**–**⌘3** to focus a pane), nestable. **Buffers are shared**, so opening the same file in two panes still means one buffer and an edit in one always shows up in the other; **scroll position and cursor are per pane**. Dividers drag. **The split layout is restored across restarts** (it is saved with the session)
- **💡 Quick fix (⌘.) and signature help (⇧⌘Space)**: press ⌘. on a diagnostic to get the LSP's code actions and fix it in place; open a call's parentheses and ⇧⌘Space shows the parameter hints. **The symbol under the cursor is faintly highlighted everywhere else in the file** (documentHighlight — turn it off from the palette), and a selection can be formatted on its own (rangeFormatting). Anything the server does not implement does nothing silently and tells you so
- **👤 Git blame** (off by default): `author · relative date` in the gutter. **Only the visible lines are asked for**, and asynchronously, so a big file still holds its frame rate. Click a line to open that commit's diff as a tab. Toggle from the View menu, the palette's "Toggle Git blame", or `git_blame` in `config.toml`
- **🖱 Drag tabs to reorder them**: grab a tab and move it left or right
- **🛠 `.vscode/tasks.json` support**: picked up when the project has one, and runnable from the palette's "Run build task…". **JSONC (comments, trailing commas) is fine.** It reads `label` / `type` / `command` / `args` / `options.cwd` / `options.env` / `group` / `presentation.reveal` and expands `${workspaceFolder}`, `${file}`, `${fileBasename}` and `${fileDirname}`. **A task left holding a variable we cannot expand is not dropped from the list — it is greyed out with the reason**, because a task that silently vanishes leaves you guessing why
- Git branch display, automatic Japanese UI font fallback

### 👾 Multi-agent
- Launch agent presets with one click (⌘⇧A) and run multiple sessions in parallel
- Per-session status (●/○), uptime, restart, force-kill
- **29 CLI agents are recognized by a built-in catalog**: Claude Code / Codex / Grok / Cursor / GitHub Copilot / OpenCode / MiMo Code / Amp / OpenClaude / Antigravity / Pi / oh-my-pi / Hermes / Devin / Goose / Auggie / Autohand / Crush / Cline / Command Code / Continue / Droid / Kilo Code / Kimi / Kiro / Mistral Vibe / Qwen Code / Rovo Dev / Aider
- **`Agent +` opens the catalog picker** — a searchable list you add from. Agents already installed on your machine sort to the top; the ones that aren't show you the install command instead of failing silently
- Permission modes (🛡 Approve / ⚡ Full-auto) auto-apply to every agent in the catalog. **You never have to write the flags in your preset** — and for Goose and Aider, which have no blanket auto-approve flag at all, the same mode is applied through environment variables instead
- A CLI agent that isn't in the catalog still runs in parallel — just register it as a preset
- Push permission-mode changes to running sessions via each row's 🛡 button (or "🛡 switch all")
- **Unread markers (◆)**: a session gets a ◆ when it produced *semantically* new output since you last looked (spinner ticks and elapsed-time counters don't count). Shown on tabs, the sidebar, and Cockpit cells alike; right-click "📩 mark unread" to pin one for later
- **Rate-limit detection (⏳)**: warnings like `usage limit reached` are detected on screen and surfaced as a badge + notification. **A rate-limited session is never assigned new tasks**; it rejoins automatically once the limit clears
- **Account/profile switching**: put `CLAUDE_CONFIG_DIR` / `CODEX_HOME` etc. in a preset's `env` to run **the same CLI under different accounts (subscriptions) in parallel** (a leading `~/` in values expands to your home directory)
- **📜 Persistent terminal logs**: each session's raw output is kept under `~/.zaivern/term_logs/` (4 MB rotation, newest 40 files), and the 📜 menu on the terminal panel reopens last session's log — "how far did it get last night?" survives a restart
- **💬 Chat history is resumed only when you ask for it**: launching Zaivern starts **nothing** on its own. The single way back into an earlier conversation is the 💬 sessions tab. Set `restore_agents = true` in `config.toml` and the old behaviour returns: previous agent tabs are recreated, the last 1 MB of scrollback is replayed behind a `── 前回のセッションここまで / 再開します ──` divider, and **claude is relaunched with `--continue`, codex with `resume --last`**. **Environment variables are deliberately not persisted** (they can hold secrets); they are re-read from the matching preset in your current config
- **✉ A multiline composer per agent**: the input box under the Cockpit is addressed to one agent, and **Enter inserts a newline — ⌘ (Ctrl on Windows / Linux) + Enter sends**. Drafts are kept per recipient, so a half-written review no longer flies off to everyone the moment you press Enter
- **💬 Past-session sidebar / 📊 plan usage**: pick a session claude or codex left behind and **resume it in that folder** (`--resume <id>` is added for you). The list shows **only conversations held in the folder you have open** — there is no per-branch (worktree) grouping, so the list stays the same for as long as the folder does (the same scoping the VS Code Claude Code extension uses). The status bar carries a usage estimate; click it for the breakdown (per account, measured values marked apart from projections, plus a run-out estimate). The tally is computed entirely offline
- **🔢 Numbered menus and surveys don't stall you**: prompts like `1. Yes` or `Select an option [1-3]:` are detected and answered automatically (skip → affirmative → most-positive end of a rating scale). The fact that it answered is recorded as "auto-answered the survey: n" in the session events and the audit log. Meaningful open choices and privilege escalation are never auto-answered — they surface as pending approvals instead
- **📋 Paste a clipboard image straight in**: press **⌘V** (**Ctrl+V** on Windows / Linux) in an agent terminal. If the clipboard holds an image it is written as a PNG into `zaivern-clip/` in your temp directory and `@path` lands in the input box — **nothing is submitted**. If the clipboard holds text, normal text paste wins as before, so nothing you already do changes. Saved PNGs are pruned automatically to the newest 24

### 🔌 MCP server manager — "where did I write that one down?" in a single table
The palette's **"🔌 Manage MCP servers"**. Every CLI keeps its MCP servers in a different file, so from where you sit the question "which file holds this server, and which agent does it actually reach?" has no answer. This panel folds them into one.

- It scans the workspace's **`.mcp.json` / `.cursor/mcp.json` / `.vscode/mcp.json`** and your home's **`~/.claude.json` / `~/.codex/config.toml` / `~/.gemini/settings.json` / `~/.cursor/mcp.json`**, and every row says which file it came from and which agent it reaches
- **The values of `env` and `headers` are never displayed** — they never even enter the struct (what you do not hold cannot leak). You see **key names and whether they are set**, nothing more, and URLs are drawn with their query string and userinfo stripped. "No value is on screen" is pinned by a test on the single function that builds the displayed strings
- Enabling / disabling edits the `disabled` key **surgically in the raw text**, so key order and comments survive (the result is re-parsed to confirm). **TOML is read-only** here, because there is no format-preserving writer for it
- Broken JSON does not crash anything: the reason it could not be read becomes the row's state (never swallowed). Scanning happens **only when you open the panel** — `~/.claude.json` runs to 100 KB, and reading that every frame is out of the question

### 🧩 Skills / slash command manager — the "I forgot where I put it" problem
The palette's **"🧩 Manage skills / commands"**. `.claude/skills/<name>/SKILL.md` (a described prompt the agent loads when it becomes relevant) and `.claude/commands/<name>.md` (a prompt `/<name>` expands to) are folded into one table spanning **all three tiers: project, user and plugin**.

- The YAML front matter's `description` is read by **a pure function of our own** (no new dependency). An unterminated `---`, a colon inside a value, an empty file, a huge one — none of it panics; it all falls back to "there was no front matter"
- **This panel writes to no file at all** (read-only). Claude Code has no enabled/disabled concept for these, so neither do we
- A skill offers **copy path**, not "send". Plugin-provided skills are addressed as `plugin:name` and some are loaded by description match, so there is no keystroke that reliably invokes one — firing a guessed `/name` would just fail silently. Slash commands *are* invoked as `/name`, so those are handed over as commands
- Scanning happens only when you open it (a plugin tree runs to hundreds of directories)

### 🔁 Automatic account failover on rate limits (off by default)
So an unattended overnight run does not stop dead the moment one quota runs out. **Disabled by default** and only ever active when you turn it on — moving to another account is also moving where the bill lands. Enable it from the command palette.

- **What convinced it there is a rate limit is always on screen.** The ladder is climbed down in order: **structured protocol > vendor-provided hook > vendor state file > screen scrape**. When the bottom rung (the screen) is the only evidence, it is **labelled an estimate** and will not switch until a corroboration pass — word-sequence match, repeated match, and **the raw output genuinely not advancing** — agrees (design principle 4)
- The state machine is five stages — `detect → pick a candidate → switch → resume → verify` — and the UI shows **which stage it is on**
- **The current session is never killed.** A new session is launched under the new profile, that is all (there is no path here that fires a kill at an exited session)
- Four independent brakes stop it retrying forever: a per-session switch cap, a per-candidate attempt cap, exponential backoff, and **a preset already tried in this chain is never chosen again**

### 📋 Fleet board — eight lanes, everyone's "right now"
**⌘⇧K** (the "フリート看板" menu item, or the "📋 看板" tab at the top). Every running agent becomes a card, sorted into the lane matching its state, under KPI tiles for running / working / needs-you / done. It is a **full-window centre view**, on par with the Cockpit and the deck.

- Eight lanes: **idle / thinking / editing / running / verifying / waiting for approval / stalled-or-broken / finished**. A card must hold a new state for 400 ms before it moves (approval, stalls, and completion move instantly), and it glows briefly when it lands
- Each card carries: an activity chip, how long the current state has lasted, the file it touched most recently, the last command, the newest output line, and an output pulse for the last 30 seconds
- **Guesses are never dressed up as facts.** Classification walks down from the strongest signal: is the process alive → approval-prompt detection → rate-limit detection → the supervisor's own verdict → screen text. Anything read off the screen is marked `≈`, anything with hard evidence `✓`, and hovering shows you which. When the supervisor says a session is idle, stale screen text is not allowed to claim it's still running
- Approve ✅ / deny ❌, send an instruction, switch permission mode, restart, or kill — straight from the card. The box at the top broadcasts to everyone
- **Vertical mode** (auto / horizontal / vertical from the menu; your choice is remembered). Select a card and its terminal goes live underneath: ↑↓ / j / k to move, Enter to type into it, Esc back to the board
- Watching costs nothing: PTY sampling runs at 150 ms while output flows and 1 s when it doesn't, and repaint requests relax from 33 ms during animation to 1 s at rest and 2 s once every agent has exited

### 🏁 Prompt fan-out race — one instruction, N agents, side by side
Open it from the command palette: **"🏁 プロンプトレース (1 プロンプトを複数エージェントで並走)"**. The point isn't which agent is fastest — it's being able to pick the best answer afterwards.

- Write one prompt, choose **2 to 4** presets. Each racer gets its **own work tree** via `git worktree add -b race/<slug>-<i>` (created next to the repo as `<repo>-race-<slug>-<i>`), the agent launches with that directory as its cwd, and the prompt is delivered there
- A race **refuses to start** on a dirty work tree or a detached HEAD. If a worktree fails to be created partway through, the ones already created are rolled back before it stops
- The dashboard lists each racer's state (preparing / 🏃 running / ⏹ finished / ✅ adopted / 🗑 discarded) alongside its ± diff stat, refreshed every 4 seconds **on a background thread** — git never runs on the UI thread. `Diff` opens a read-only diff tab
- **Adopt** is `git merge --no-edit`; on conflict it simply `merge --abort`s — nothing is forced. **Discard** is `git worktree remove` + `git branch -D`, leaving nothing behind
- **You find out about collisions while they race, not at review time.** The set of files each racer has touched (uncommitted *and* committed) is compared on the same 4-second cycle; two or more racers touching the same file surfaces a `⚠ N ファイルが2体以上で競合` summary plus a per-row `⚠ 衝突 a.rs, b.rs …` badge. No overlap and you get `✓ 単独`
- Adopting a second racer whose files overlap one you already adopted **stops for a confirmation** instead of merging quietly. Click again and it proceeds — the guard is there to make you notice, not to forbid you

### 🔔 Notifications + sounds
- Approval-wait, success (✅), and failure (❌ + exit code) announced via popup + OS-native sounds (can be turned off)
- When the window is unfocused, notifications also go to macOS Notification Center (Linux: notify-send)
- **Webhook push for when you're out**: set `webhook_url` in `config.toml` to an [ntfy](https://ntfy.sh) topic URL or a Slack / Discord incoming webhook and approval-waits, exits, and rate limits are POSTed there. With ntfy, subscribing on your phone is all it takes — **even outside your LAN you'll know when you're needed** (the phone remote itself stays LAN-only)

### 🐾 Desktop pet "Zaigani"
- Blinks, follows your cursor with its eyes, wanders around; dozes off when idle → deep sleep (💤), startled hop when you come back
- Agent-linked reactions: marching "⚙ n" while agents run (faster with more agents), grooving (🎵) at 3+, fidgeting on approval-wait, 🎉 on success / 💥 on failure
- 💬 Approval bubble: ✔ Approve / ✖ Deny / Open with one click (keys sent to the PTY are customizable via `pet_approve_keys` / `pet_deny_keys`). One prompt, one answer — a prompt still lingering on screen won't re-summon the bubble or re-send the keys
- ⚡ Auto-YES (off by default): one switch in the 🐾 menu flips approval prompts to automatic answers. While it's off, the final YES is always yours
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
| 🔤 `syntax-pack` | **Syntax highlighting for the major programming languages.** Adds **53 languages** missing from the built-in set — TypeScript / Kotlin / Swift / Dart / Zig / Elixir / Julia / Solidity / TOML / Dockerfile / Terraform / PowerShell and more — and resolves aliases such as `.tsx` `.vue` `.mjs` `.json5` (definitions are plain TOML, so **you can add your own language**) |
| 🌐 `english-mode` | Language pack that switches the UI to English (the only one that starts disabled; toggle it in the 🔌 tab — English instantly, back to Japanese when off. The `lang/*.toml` dictionaries are editable) |

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
- **🔤 Syntax highlighting**: bundle language definitions (TOML) with `[[syntax]]`. One language = one
  block: comment markers, quotes and a keyword table are all it takes. Colors come from the active
  color theme's scopes, so added languages follow your theme
  (→ [Add a language](#-add-a-language))

**Drive the app itself**: set `output = "actions"` and every line of stdout becomes an instruction (JSON Lines).

```sh
echo '{"action":"open_file","path":"src/main.rs","line":42}'
echo '{"action":"agent_prompt","agent":"claude","text":"write tests for this function"}'
```

Open files, notify, open tabs, run things in the terminal, rewrite a panel, **talk to an agent** — all fair game. `agent_prompt` **only places the text in the input box** unless you explicitly pass `submit`, so nothing runs off on its own.

Three buttons to manage it all: **➕ New** (generates a full sample template), **📤 Export** (writes a `.zvplug` you can hand to anyone), **📦 Install** (just pick a received `.zvplug` / `.zip`).

#### 🔤 Add a language

`syntax-pack` ships by default, so **TypeScript, Kotlin and Zig are colored out of the box**.
Anything missing is one TOML block away — no Rust, no rebuild.

```toml
# ~/.zaivern/plugins/my-langs/syntaxes/mylang.toml
[[syntax]]
name = "MyLang"
extensions = ["ml2", "mylang"]     # extensions (no dot)
filenames = ["mylangfile"]         # or match files that have no extension
line_comment = ["#"]               # line comments
block_comment = [["/*", "*/"]]     # block comments (tracked across lines)
doc_comment = ["##"]               # doc comments get their own color
strings = ["\"", "'"]               # quotes
multiline_strings = [['"""', '"""']]
char_literal = true                # treat 'a' as a character literal
ident_extra = "$-"                 # extra characters allowed in identifiers
attribute = "@"                    # annotation / decorator prefix
preproc = ["#"]                    # preprocessor directives at line start
fold = "brackets"                  # brackets | indent | markdown (folding strategy)
case_sensitive = true
keywords  = ["if", "else", "loop"]
types     = ["int", "str"]
constants = ["true", "false", "nil"]
builtins  = ["print"]

# If an existing syntax already fits, alias it instead of writing a new one
[[alias]]
target = "HTML"
extensions = ["myhtml"]
```

```toml
# ~/.zaivern/plugins/my-langs/plugin.toml
[plugin]
name = "my-langs"
version = "0.1.0"
api = 3

[[syntax]]
path = "syntaxes"        # a file or a directory
```

- Detection order is **plugin → built-in (syntect) → first line (shebang)**, so a plugin can also
  override a built-in mapping.
- Folding, indent guides and `⌘/` comment toggling all follow from the same definition.
- The plugin row in the 🔌 tab shows **🔤 and the language count** (hover for the list).

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
zai status                         # list running Zaivern instances (PID / version / uptime / workspace)
zai status --json                  # the same list as JSON
zai plugin list                    # list plugins
zai plugin new <name>              # scaffold one
zai app install                    # register in the OS app list (Launchpad / menu / Start Menu)
zai app uninstall                  # remove that registration
zai firewall status                # check the 📱 inbound rule (Windows)
zai firewall allow                 # allow 📱 inbound (TCP 8899-8919, admin)
zai firewall revoke                # remove the inbound rule
```

**Headless paths that work with no editor running.** CI, cron, or another agent can drive worktrees and sessions without bringing up a GUI.

```bash
zai worktree create <branch> [--from <base>]  # create under .claude/worktrees/, print the absolute path
zai worktree list [--json]                    # branch → HEAD → state → path
zai worktree remove <branch> [--force]        # remove the worktree (the branch itself stays)
zai session list [--json]                     # id / state / agent / last update / working folder
zai session send <id> <text>                  # send to a running session (an exited one errors, changes nothing)
zai session log <id> [--tail N]               # the last N lines of raw log (default 50)
zai agent list [--json]                       # name / installed? / launch command / resolved path
```

**Every one of them takes `--json`**, and the exit codes are the same across all subcommands — **0 = success, 1 = runtime error, 2 = bad arguments**. A shell can tell "I typed it wrong" apart from "it ran and failed", so scripts do not have to swallow either (that mapping is pinned by a test that spans the subcommands). `zai agent list` walks `PATH` itself, so it answers the same way on machines without `which` (and consults `PATHEXT` on Windows).

Bare `zai` and `zai .` still launch the GUI, exactly as before. **Launched with no folder argument, Zaivern reopens the folder you had last time** — it walks the most-recently-used list in `~/.zaivern/menu_state.toml` and takes the first entry that still exists on disk. Want the current directory every time? Pass `zai --no-restore`, or set `ZAIVERN_NO_RESTORE=1`.

**Detecting a running instance.** With no arguments, `zai status` / `zai status --json` reads the registry at `~/.zaivern/instances/<pid>.json` and **lists the Zaivern Code instances running right now**. Exit code is **0** when at least one is alive and **1** when none is, so a shell can branch on it directly: `zai status >/dev/null || zai .`. Liveness is checked per OS (Linux `/proc`, macOS `kill(pid, 0)`, Windows `tasklist`) and **matched against a start-time signature so a recycled PID never counts as alive**. `zai status "..."` with a string is unchanged — that still writes to the status bar.

### 🔤 Language servers (LSP)
If `rust-analyzer` / `typescript-language-server` / `pyright-langserver` / `gopls` is on your PATH it starts automatically and shows diagnostics (errors/warnings). The line-number gutter turns red/yellow, and the status bar shows `⛔count ⚠count`. Editing works normally even without any server installed.

Diagnostics are not all of it — **the usual VS Code keys are wired up**: **⌘I** completion, hover for types and docs, **⇧F12** find references, **⇧⌘O** go to symbol, **F2** rename (edits spanning several files are applied together), **⇧⌥F** format document, **⌘.** quick fix (code actions), **⇧⌘Space** signature help. **The symbol under the cursor is faintly highlighted wherever else it appears** (documentHighlight — turn it off from the palette), and **a selection can be formatted on its own** (rangeFormatting). Turn on "🛠 Format on save" in the toolbar and every save formats first (off by default). When a server doesn't implement a capability, it says "this server doesn't support …" rather than silently doing nothing.

> **Completion moved from `⌃Space` to `⌘I`.** macOS reserves `⌃Space` for "previous input source", so **it never reaches the app**. Rather than ship a default that shows up in the UI and can never fire, the default is now `⌘I` — VS Code's own second binding on the Mac. `⌃Space` still works wherever it does reach the app.

Setup examples: `rustup component add rust-analyzer` / `npm i -g typescript-language-server typescript` / `npm i -g pyright` / `go install golang.org/x/tools/gopls@latest`

### ⌨️ Japanese input (IME)
Type Japanese directly inside the terminal. Uncommitted composition text is overlaid with an underline at the cursor position, and only committed text is sent to the agent.

### 🌿 Git line gutter
In a git repository, line numbers are color-coded by diff (green = added, yellow = modified). The status bar shows the branch name + changed-file count (±N).

### 🌳 Git status in the file tree, and follow-the-active-file
The tree tells you where the work is happening, too.

- Changed files carry a colour-coded badge: `M` modified / `A` added / `U` untracked / `D` deleted / `R` renamed / `C` conflicted
- **Ancestor folders are tinted as well**, with a count — `📁 src  M•3` tells you there are 3 changes under it without expanding anything (the colour follows the most severe status below)
- Status comes from `git status --porcelain` refreshed on a **background thread** every 2 seconds. git is never called on the UI thread, so a big repository doesn't cost you frames
- **🎯 Follow the active file** (on by default): switch tabs in the editor and the tree expands and scrolls to that file. Right-click a workspace root → "🎯 アクティブファイル追従: ON / OFF" to toggle it; the choice persists across restarts

### 📚 Multi-folder workspace
Open several folders at once. List them as arguments — `zai frontend backend shared` — or add one later from the command palette's "Add folder to workspace".

- The file tree lists each root under its own heading (with a single root, it looks exactly as it always did)
- File search spans every root. **Only when the same relative path exists in more than one root** does Zaivern prefix the folder name to tell them apart — no noise the rest of the time
- git detects the real repository per root (`rev-parse --show-toplevel`), so opening a *subdirectory* of a repo still shows correct diffs. Two roots inside the same repository share one git state
- Session restore is keyed on the *set* of roots, so reordering them still restores the same workspace

### 🐙 GitHub integration
List pull requests and issues, read a PR's diff, and switch branches — all through the `gh` command. **No extra auth setup**: if you've run `gh auth login`, it already works. On a machine without `gh`, the panel is disabled cleanly rather than erroring at you.

A PR diff opens as a read-only tab rendered in the **side-by-side diff view**, same as VS Code. See the next section.

### ⇋ Diff view (VS Code parity)
PR diff tabs, race diff tabs and Git's "review changes" pane all go through **the same renderer**, so a diff reads the same wherever you find it.

- **Side-by-side by default.** Before on the left, after on the right, with matching lines always at the same height (a line that exists on only one side gets an empty placeholder opposite it). Both columns are painted as one row, so **the two sides cannot drift out of sync** — and no repaint is needed to keep them together
- **Falls back to one column when narrow.** Below ~240px (≒ 32 columns) of code per side it degrades to inline automatically, so it never gets cut off in a sidebar or a shrunken window. Toggle it from the **⇋ / ≡** button in the diff toolbar, the "toggle diff view" command in the palette, or `diff_view = "side_by_side" | "inline"` in `config.toml`
- **Word-level highlighting.** A replaced line tints only the part that actually changed — `count` → `counter` highlights just the added `er`. Splitting happens at **grapheme-cluster** boundaries, so Japanese (no spaces) and emoji (ZWJ families, skin-tone modifiers, flags) never break. Pathologically long lines fall back to tinting the whole line rather than stalling the UI
- **F7 / ⇧F7 jump to the next / previous change**, wrapping around at the end. With nothing to jump to it stays put and just says "no changes"
- **Unchanged lines collapse.** Long runs of context keep 3 lines on each side and fold the middle into "⋯ N lines", click to expand
- **Syntax highlighting inside the diff.** The language comes from the file name, and colours are cached per file (recomputed only when the diff content changes). Huge diffs skip highlighting and render plain
- The `+N` / `-M` badges in the file header are **omitted when zero** (no `+0 -0` on a rename-only file)

**⚡ Start on an issue in one click.** Pick an agent from an issue's "⚡ 着手" (start) menu and Zaivern will (1) create a dedicated git worktree next to the repo (branch `wt/issue-N`, same convention as the worktrees plugin), (2) add it to the workspace, (3) launch the agent with that directory as its working dir, and (4) drop a kick-off instruction into its input field once the session has settled. **You still press Enter** — so you can edit the instruction before it runs.

### 🔎 Review your changes — read them like a PR before you open one
"Review changes" in the Git panel (or "Review changes (PR-style local review)" in the command palette) puts **your not-yet-pushed local changes** into the same shape as a GitHub PR.

- Changed files list on the left, grouped by directory, with the same colors and badges as the tree, `+N −M`, and "N files changed · +X −Y" on top. Click to jump to that diff, `n` / `p` to step between files. Compare against **working tree vs HEAD / staged only / unstaged only / any revision** — untracked files are synthesized as all-additions, binaries show no content, and oversized diffs are truncated and say so
- Per file: stage, unstage, **discard (two-step confirm)**, open in the editor. Whitespace-insensitive mode and 3 / 10 / full context lines are toggles. git runs on a dedicated thread behind a 5-second cache, so a big diff never costs you frames

### 💬 Inline review comments on a diff
In a PR diff tab, a race diff tab, or the PR-style review above, **click a line and write a comment right there**.

- A comment is anchored to "file + old/new side + line number", so re-parsing the diff or scrolling past it (the view is virtualized) never loses its place
- Per thread: resolve, un-resolve, edit, delete. The toolbar reads "レビューコメント n 件 (未解決 m 件)" — n comments, m unresolved
- Unresolved comments **collapse into a single prompt**: a `以下のレビューコメントに対応してください:` header followed by `@path:line` (marked `(削除行)` for removed lines), the quoted line, and your text — ordered by file, then by line. `コピー` copies it out, or **"send to agent" drops it into the composer draft** for whichever agent you were reviewing for — **it only fills the box, it never presses Enter**. **Resolved threads drop out automatically**, so you never re-send a note you already dealt with

### 🧭 Open in an external IDE
Send the file you're editing to another editor **with your cursor line intact**. VS Code / Cursor / Zed / Trae / Kiro / Sublime / the JetBrains family / Xcode / Fleet / Neovide / Emacs are supported.

Installed IDEs are detected automatically, and only those are offered. If your `code` command has been hijacked by some other product, Zaivern resolves the real binary and identifies it correctly. Apps that ship no CLI are handed the file over their URL scheme.

### 🔭 Agent supervision and hand-off
Run several agents and sooner or later one goes quiet, repeats the same failure, or dies. This is the layer that notices and does something about it.

- **Detection**: stalling (silence with no progress), looping (the same output over and over), a storm of errors, abnormal exit, an approval prompt left unattended, runaway output. Spinners and counters are correctly treated as *not* progress
- **The watchdog detects and notifies — nothing more**: it never auto-approves, nudges, restarts, or stops an agent on its own. Anomalies surface to you as badges, toasts, and notifications, and you decide the move. **There is no code path that silently types into an agent's input box**
- **Reassignment**: a stalled task is handed to a different agent. An agent that already failed a task never gets it back. **The hand-off does not happen until the previous holder is confirmed stopped** — otherwise two agents edit the same files. When the retry budget is spent, it escalates to you instead of looping forever
- **Inter-agent messaging**: a message is delivered only when the recipient is idle, because interrupting mid-generation corrupts its input. Hop limits, rate limits, and round-trip detection all apply, and any message that couldn't be delivered is recorded with the reason

**Hand out the work.** Create a "task" from the cockpit and either assign it to a specific agent or let auto-assignment decide. The list shows state, owner, and attempt count, and anything the system gave up on shows as a red `NeedsUser`. If an assignment is refused, you get the reason in plain words — nothing is worse than silence.

**Let the agents talk to each other.** Besides sending by hand, an agent can write this at the start of a line and it goes straight to the target:

```
[ZAI-TO:backend] the migration passed, go ahead on the API side
[ZAI-TO:ALL] moved the shared type definitions into types.ts
```

There is no LLM guessing at which sentences look like they're addressed to someone. **Only the line-start marker counts — deterministic, on purpose.** And when an injected message is echoed back onto the screen, it does not get re-sent as a new message (get that wrong and messages multiply without end).

### 🛡 Unified approval queue — every "may I?" in one line
With five agents running, approval prompts scatter across five terminals. This folds them into one queue: the **"🛡 Approvals" tab** in the panel (with a pending-count badge; "Open approval queue" in the command palette gets there too).

- Requests are classified into nine kinds: read / write / delete / shell command / network / git operation / package install / **privilege escalation** / other. The queue is fully keyboard-drivable
- **Policies** declare "this kind, in this scope, is always allowed / always denied". Scopes are `global`, `agent`, `session`, `path`, and **the more specific one wins** (a rule on `/repo/src/secret` overrides one on `/repo`). Put them in `[[approval_policies]]` in `config.toml` and they apply from startup. **Privilege escalation, though, can never be auto-approved** — writing `allow_always` for it is rejected and you get a one-time approval instead
- Every decision lands in an append-only audit log at `~/.zaivern/approvals.jsonl` (kind plus the first 160 characters of the target — never the full command or anything pasted into it). The panel can read the tail

### 💡 Super Agent (Commander) — give the watchdog a brain
The supervision itself runs on Rust rules. You don't need an LLM to spot a stall or a loop, and **if the watchdog is an LLM, nobody is left to notice when the watchdog breaks.** So detection stays deterministic code.

On top of that, you can hand the question code is bad at — *"okay, so what's the right move here?"* — to an AI. In the cockpit's **💡 Super Agent**, you just **name one of your running agents as the "commander"**. The commander writes `@target: instruction` on its screen (`@all:` for everyone) — and that is the entire protocol.

- The default is **"none"**. Name nobody and no commanding ever happens
- **Any running agent can be named**, and you can swap commanders mid-flight. The one exception is a plain shell — echoed command output is too easy to misread as a directive, so it isn't offered in the first place
- A commander's directive reaches you as a **📮 notification**. **It is never written into any agent's input box automatically** — whether it actually goes out is your call (Cockpit broadcast, or typing it into a terminal yourself). Auto-injection existed once, and was removed: text flowing into a box you're typing in is not a feature
- Notification bodies pass through **redaction and a length cap**. A line that can't be parsed does **nothing**. A watchdog that manufactures actions out of ambiguous answers is more dangerous than one that stays quiet
- Destructive operations simply cannot be expressed. A commander has no way to restart or stop anyone
- **The commander is itself supervised, like everyone else.** Exempt it and you've built a single point of failure
- **The commander can keep doing normal work.** You don't have to burn a slot on supervision alone

### 💾 Session restore
On restart, the previous tabs, active tab, and panel state are restored automatically per workspace (`~/.zaivern/sessions/`). Agent tabs are **not** brought back unless you opt in with `restore_agents = true` (default `false`).

**The folder comes back as well.** Start `zai` with no folder argument and it walks the most-recently-used list in `~/.zaivern/menu_state.toml` and reopens **the first entry that still exists**. To always start in the current directory instead, use `zai --no-restore` or `ZAIVERN_NO_RESTORE=1`.

### 🧯 Recovering from internal errors (and the Windows freeze, fixed at the root)
- A panic while painting **does not take the app down with it**. If the same place breaks 3 times inside a 10-second window, **only that piece is dropped from rendering** and everything else keeps running. A banner appears at the top saying rendering was stopped for that area, with `閉じる` to dismiss and `再試行` to lift the quarantine and try painting it again; the dropped area shows a placeholder in its place. Details land in `~/.zaivern/panic.log` (rotated to `panic.log.old`)
- **Quarantines decay.** After 300 clean frames with an empty window the memory resets, so running for hours never means slowly losing features to one incident long ago. It gives up only on 3 consecutive panics, or after more than 3 quarantines
- **The Windows Cockpit freeze was closed off from three sides**: (1) ConPTY resize requests are coalesced — **the same size must hold for 2 frames before one request is sent**, and a dedicated thread keeps only the latest value so the UI thread never waits on it; (2) file dialogs run on a worker thread, so the UI keeps painting while one is open (macOS stays synchronous — NSOpenPanel is main-thread-only); (3) the panic quarantine above. **Save-as holds a buffer ID, not a tab index**, so reordering tabs while the dialog is open can't redirect the write to a different file
- **The "black tile" terminal was fixed at the source.** Panics in the vt100 parser (scroll regions, tab stops, line operations) and runaway `CSI <huge number>` repeat parameters are now clamped — the patches live in `vendor/vt100` and are pinned by a hostile-input test suite. A tile that still breaks is quarantined on its own with a "⚠ retry" banner, **never left as an empty black rectangle**. Closed alongside it: plugin timeouts now kill the whole process tree (a stray grandchild used to freeze the editor), the IME confirm-Enter that leaked through to the agent, and corruption when saving in non-UTF-8 encodings such as CP932

### 📱 Phone Remote in detail
- **What you can do**: view/edit/save open files, switch tabs, search & open workspace files, view agent terminals, send instructions, approve (Enter / Esc / ^C / ↑ / ↓ / Tab / ⇧Tab / 1 / 2 / 3 / y buttons), and run commands (save, new file, Cockpit, zoom ±, approval-mode switch, and more)
- **How it works**: a tiny built-in HTTP server (port 8899, auto-fallback to 8900–8919 if busy). Pure `std::net` — zero extra crates
- **Security**: authenticated with a random token generated per launch (embedded in the QR URL). Tokenless API access gets a 401. LAN only
- **🔐 Reach it without sharing a Wi-Fi — SSH reverse tunnel**: LAN mode assumes you are on the same Wi-Fi, so it does not reach you away from home. 📱 → "Remote connection (SSH)" relays through **a host you can already SSH into** (a VPS, a home server, the office jump box). The path is `phone ──HTTP──▶ jump box:8899 ──SSH tunnel──▶ PC:127.0.0.1:8899`, so **the phone needs no SSH client at all** — it just opens a URL
  - **While the tunnel is up, the server binds to `127.0.0.1` only.** The cleartext LAN listener is closed before the tunnel opens, so you never end up with an encrypted path and a raw one sitting next to it
  - **No password is entered and none is stored.** Authentication is left entirely to the OS's `ssh`, invoked with `BatchMode=yes`: with no key (ssh-agent / `~/.ssh/config`) it **fails immediately with a reason** rather than hanging on a prompt
  - **The app does not choose the bind address on the jump box.** No bind address is written into `-R`, so OpenSSH's default applies (the jump box's loopback only). Whether that gets exposed to the internet is your call, made in the jump box's `sshd_config` via `GatewayPorts`
  - Raw stderr never reaches the screen; it is folded into the point — "no OpenSSH client found", "key authentication was refused", "this host wants an interactive password" — first. Monitoring runs only while connected, so idle cost does not move
- **Windows inbound rule**: Windows blocks inbound by default, so without a rule the phone cannot connect. The 📱 panel checks this itself and offers "🛡 Allow inbound (administrator)" when it is missing. The rule it creates is scoped to **this `zai.exe` + TCP 8899-8919**, with the Domain/Private profiles — plus **Public only when the network you are on is classified as Public** (home Wi-Fi is often classified that way, and leaving Public out would not fix anything). That fact is stated in the panel, so don't use 📱 on public Wi-Fi. Any **block rule** for the executable (created if you once hit "Cancel" on the Windows prompt) **is removed when allowing**, since Windows lets block win over allow. Revoke from the panel or with `zai firewall revoke`. The installer creates the rule on the spot when it is running elevated, and otherwise only prints guidance (it never elevates on its own)

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

### Tests

```bash
cargo test        # everything, locally, on macOS / Windows / a real Linux box
```

To reproduce the CI run on your own machine, use [cargo-nextest](https://nexte.st/):

```bash
cargo install cargo-nextest --locked
cargo nextest run --locked --no-fail-fast --profile ci
```

The `ci` profile lives in `.config/nextest.toml`: it terminates any test after 60 s (45 s for the real-PTY ones) and prints failures immediately *and* again in a final summary. The **real-PTY tests** — the four modules that spawn actual shells, `terminal::pty_tests` / `pty_writer_tests` / `reap_pty_tests` / `pty_resize_tests` — form a `pty` test group that is **serialized onto a single thread**.

GitHub Actions runs **6 jobs in parallel: 3 OSes × `fast` / `pty`**, splitting those two groups into separate jobs.

```bash
# fast: everything except the real-PTY tests (parallel)
cargo nextest run --locked --no-fail-fast --profile ci \
  -E 'not test(/^terminal::(pty_tests|pty_writer_tests|reap_pty_tests|pty_resize_tests)::/)'

# pty: only the real-PTY tests (serialized)
cargo nextest run --locked --no-fail-fast --profile ci \
  -E 'test(/^terminal::(pty_tests|pty_writer_tests|reap_pty_tests|pty_resize_tests)::/)'
```

The split isn't about Linux disliking PTYs. On GitHub's hosted Linux runner (2 cores / 7 GB), spawning real PTYs in parallel exhausts the box and **kills the runner process itself, leaving no logs and no artifacts**. Only those four modules are serialized (18 tests on Linux / macOS, 12 on Windows where `pty_tests` is Unix-only); the rest of `terminal::` runs in parallel in the `fast` job. `--no-fail-fast` is there so a single run surfaces *every* failure rather than the first one.

> This section used to tell you to run `cargo test -- --skip terminal::` on Linux. **That advice is obsolete.**

**Formatting and lints are gated in CI too.**

```bash
cargo fmt --all --check   # if it fails, run `cargo fmt --all` and commit
cargo clippy --all-targets --locked -- -D warnings   # plus the frozen -A list (same as CI)
```

`fmt` runs before clippy because its failure has the most obvious fix. Clippy denies **every rustc warning and every clippy lint** with `-D warnings`, then **freezes exactly 26 pre-existing debts** with individual `-A` flags. So **rustc warnings are at zero**, and clippy **fails on any new kind of finding** — a ratchet where the frozen 26 can shrink but never grow. (This is not "clippy is perfectly clean"; the frozen debts are still there.)

---

## Keybindings

| Key | Action |
|---|---|
| ⌘⇧C | **Toggle Agent Cockpit** |
| ⌘⇧K | **Toggle the fleet board** |
| ⌘⇧A | **Launch agent (preset #1)** |
| ⌘J or ⌘\` | Toggle terminal/agent panel |
| ⌘P (Ctrl+P) | Fuzzy-find and open a file |
| ⌘⇧P | Command palette (`>` prefix) |
| ⌘S / ⌘⇧S | Save / Save as |
| ⌘N / ⌘W | New file / Close tab |
| ⌘F | Find in file |
| ⌘/ | Toggle line comment |
| ⌘⇧D / ⌘D | Duplicate line / Select next occurrence, add caret |
| ⌥⌘[ / ⌥⌘] / ⌥⌘B | Toggle fold / Unfold all / Toggle bookmark |
| ⇧⌘T / ⌘⇧V / ⇧⌘H | Reopen closed tab / Markdown・HTML preview / Replace across workspace |
| ⌘\ / ⌥⌘\ / ⌘1–⌘3 | **Split the editor right / down / focus the nth pane** |
| F7 / ⇧F7 | Diff view: jump to next / previous change (wraps around) |
| ⌘I (⌃Space also works) / ⇧F12 / ⇧⌘O | LSP: completion / find references / go to symbol |
| ⌘. / ⇧⌘Space | LSP: quick fix (code action) / signature help |
| F2 / ⇧⌥F | LSP: rename / format document |
| ⌥↑ / ⌥↓ | Move line up / down |
| PageUp / PageDown | Cursor + scroll by one screen |
| Enter | Auto-indent (previous line's indent, extra level after `{ ( [ :`) |
| ⌘B | Toggle sidebar |
| ⌘⇧E | Show and focus the Explorer (file tree) |
| ⌘+ / ⌘- / ⌘0 | Zoom **the whole window** in / out / back to 100% |
| ⌘⌥+ / ⌘⌥- / ⌘⌥0 | Zoom **only the open file** in / out / reset |
| ⌘ + wheel (or pinch) | File-level zoom when the pointer is over the editor body, window zoom otherwise |

**Zoom has two levels.** Window zoom works like VS Code's `window.zoomLevel`: the entire
UI — sidebar, tabs, menus, and the **terminal** — scales together, and the factor is
remembered in `~/.zaivern/state.toml` (50%–300%, 13 steps). File zoom applies only to that
tab's body (and its Markdown preview); it is temporary and disappears when the tab closes.
While either is off 100%, the status bar shows `🔍 125%` / `🔎 150%` — **click it to reset**.
The View menu and the command palette offer the same actions.

### Agent terminal (macOS)

When an agent terminal has focus, the standard Mac editing keys just work.

| Key | Action |
|---|---|
| ⌘A | Select the whole screen (then ⌘C to copy) |
| ⌘F | **Search inside the terminal** (full scrollback; Enter = older / ⇧Enter = newer, Esc closes) |
| ⌘K | Clear the screen (sends Ctrl+L so TUIs redraw safely) |
| ⌘← / ⌘→ | Jump to start / end of the input line |
| ⌘⌫ | Delete to the start of the line |
| ⌥← / ⌥→ | Move by word |
| ⌥⌫ | Delete the previous word |
| ⌘V | **Save the clipboard image as a PNG and drop `@path` into the input box** (plain text still pastes as text) |

### File tree (same defaults as VS Code's Explorer)

Click a row (or use the keys) to select it first. Bindings follow VS Code's per-platform defaults.

| Action | macOS | Windows / Linux |
|---|---|---|
| Rename | Enter | F2 |
| Open in editor | ⌘↓ | Enter |
| Open keeping focus / toggle folder | Space | Space |
| Copy / Cut / Paste | ⌘C / ⌘X / ⌘V | Ctrl+C / Ctrl+X / Ctrl+V |
| Cancel cut | Esc | Esc |
| Delete (with confirm dialog) | ⌘⌫ (also ⌥⌘⌫) | Delete (also Shift+Delete) |
| Copy full path | ⌥⌘C | Shift+Alt+C |
| Copy relative path | ⇧⌥⌘C | (context menu) |
| Move up/down / first/last | ↑↓ / Home・End | ↑↓ / Home・End |
| Collapse & to parent / expand & to first child | ← / → | ← / → |
| Collapse all | ⌘← | Ctrl+← |
| Type-ahead (jump by name) | just type | just type |

Pasting onto an existing name auto-renames the VS Code way: `file copy.ts` → `file copy 2.ts` → … Items pending cut are shown dimmed.

On Windows / Linux, read ⌘ as Ctrl. Inside the terminal, control keys like Ctrl+C, arrows, Tab, and Esc go straight to the PTY (Shift/Option+Enter is sent as a newline, supporting Claude Code's multi-line input).

Every shortcut can be overridden in `config.toml` under `[keybindings]` (`save = "cmd+s"` format). Action names: `save` `save_as` `save_all` `close_tab` `new_file` `new_window` `open_file` `palette_files` `palette_commands` `toggle_terminal` `new_terminal` `toggle_sidebar` `find` `global_search` `global_replace` `open_replace` `goto_line` `next_tab` `prev_tab` `nav_back` `nav_forward` `goto_definition` `goto_bracket` `toggle_cockpit` `toggle_kanban` `toggle_deck` `toggle_md_preview` `toggle_problems` `toggle_fullscreen` `run_build_task` `new_agent` `zoom_in` `zoom_out` `zoom_reset` `file_zoom_in` `file_zoom_out` `file_zoom_reset` `toggle_comment` `duplicate_line` `move_line_up` `move_line_down` `focus_explorer` `toggle_fold` `unfold_all` `toggle_bookmark` `reopen_closed_tab` `lsp_completion` `lsp_references` `lsp_symbols` `lsp_rename` `lsp_format` `lsp_code_action` `lsp_signature_help` `select_next_occurrence` `split_editor_right` `split_editor_down` `focus_pane_1` `focus_pane_2` `focus_pane_3` `diff_next_change` `diff_prev_change`. Modifiers: `cmd` `ctrl` `shift` `alt` (= `option`). File-tree keys are fixed to the VS Code defaults.

**The keystrokes printed on screen are generated from this table.** Every "⌘S" in a menu, a context menu, the palette or a tooltip is looked up rather than hand-written, so rebinding an action in `[keybindings]` **changes the label too** — the UI can never claim a shortcut you no longer have. Each action is also pinned by a test that it reaches a place which actually consumes it, so **a shortcut that shows up but does nothing fails CI**. The set of keystrokes macOS holds onto (⌘Space, ⌃Space, ⌥⌘D and friends) was read off the OS rather than guessed, and the same test proves no default binding lands on one. The egui-winit 0.29 bug where **⌘⇧C / ⌘⇧V have their key-press event swallowed and replaced by Copy / Paste** is routed around by reconstructing the original keystroke from the substituted event (the Cockpit's ⌘⇧C goes through that path).

---

## Customization — `~/.zaivern/config.toml`

Generated automatically on first launch. After editing, run **"Reload settings"** from the command palette for instant effect (or open the file directly via **"Open config.toml"**).

```toml
# Theme (dark):  "zaivern-dark" "zaivern-midnight" "zaivern-nordic"
#                "zaivern-ember" "zaivern-forest" "zaivern-ocean" "zaivern-carbon"
# Theme (light): "zaivern-light" "zaivern-paper" "zaivern-daylight" "zaivern-frost"
# or a full path to a VS Code-compatible theme JSON
theme = "zaivern-dark"
editor_font_size = 15.0
terminal_font_size = 13.0
show_hidden_files = true

# Word wrap and whitespace rendering (·/→) in the editor body
# (also toggleable from the View menu and the command palette)
# word_wrap = false
# show_whitespace = false

# Minimap (zoomed-out view on the right edge) and breadcrumbs (path bar on top)
# (also toggleable from the View menu and the command palette)
# The minimap costs 64px of body width, hence off by default; it hides itself
# automatically on narrow windows
# minimap = false
# breadcrumbs = true

# Show git blame (author · relative date) in the gutter. Off by default.
# While on, only the visible range is blamed, asynchronously
# git_blame = false

# When you reopen a folder, restore the previous agent tabs and pick the
# conversation back up (previous scrollback is replayed, then claude is
# relaunched with --continue and codex with resume --last; false = don't restore)
# restore_agents = false

# Default permission mode (auto-applied to all 29 agents in the catalog)
#   "ask"   = user approval required every time (safe, default)
#   "auto"  = auto-YES to everything (bypass flags added per CLI)
#   "agent" = agent-first (use whatever flags the preset command says)
approval_mode = "ask"

# Push notifications while you're away: an ntfy topic URL or a
# Slack / Discord incoming webhook. Approval-waits, exits, and
# rate limits are POSTed there ("" = off)
# webhook_url = "https://ntfy.sh/your-topic"

# Desktop pet 🐾
show_pet = true
# pet_variant = "blocky"   # look: "blocky" | "crab" | "cat" | "cloud"
# pet_scale = 1.0          # size: 0.75=S / 1.0=M / 1.4=L
# pet_free_roam = true     # wanders around
# pet_sleep = true         # sleeps when idle
# pet_sounds = true        # sound effects
# pet_bubbles = true       # approval bubble
# pet_auto_yes = false     # auto-YES to approval prompts (off = you approve)
# pet_approve_keys = "\r"    # keys sent to the PTY on approve (Enter)
# pet_deny_keys = "\u001B"   # keys sent to the PTY on deny (ESC)

# ── Unified approval queue policies (empty by default = you are asked) ──
# [[approval_policies]]
# kind = "file_read"         # file_read/file_write/file_delete/shell_command/
#                            # network_access/git_operation/package_install/
#                            # privilege (always manual)/other
# scope = "agent"            # "global"|"agent"|"session"|"path" — most specific wins
# target = "claude"          # what the scope refers to (empty for global)
# decision = "allow_always"  # "ask"|"allow_once"|"allow_always"|"deny_always"

# ── Extra auto-YES rules (evaluated before the bundled table) ──
# When a CLI rewords its approval prompt, fix it here without rebuilding
# [[auto_yes_rules]]
# pattern = "Do you want to proceed?"  # on a match, reply is sent to the PTY
# reply = "\r"                         # agent = "" (default) means all agents

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
├── theme.rs         11 themes (7 dark / 4 light) + egui style application
├── theme_json.rs    Color-theme JSON import (VS Code-compatible)
├── config.rs        ~/.zaivern/config.toml loading, generation, project overrides
├── editor.rs        Buffer & tab management
├── editor_ops.rs    Pure text-editing operations (multibyte-safe)
├── editor_split.rs  Editor splits (reuses the terminal's split tree, shared buffers, pure layout)
├── minimap.rs       Minimap (zoomed-out view, click/drag scroll, hit/diagnostic/bookmark marks)
├── breadcrumb.rs    Breadcrumbs (path part needs no LSP; middle elided when it does not fit)
├── preview.rs       Hex viewer / video-audio info cards / ZIP listing + magic-number sniffing
├── tasks.rs         `.vscode/tasks.json` (JSONC) reading — pure functions throughout
├── highlight.rs     syntect → egui LayoutJob conversion (hash-cached)
├── snippets.rs      VS Code-compatible snippet parsing & Tab expansion + Emmet
├── file_tree.rs     Lazy-loading file tree (multi-root) + context menu
├── fuzzy.rs         Fuzzy-match scoring
├── palette.rs       Command palette state & action definitions
├── keybinds.rs      Customizable keybindings
├── git.rs           git CLI integration (repo detection, branch, per-line diff marks)
├── git_panel.rs     Git side panel (list and switch branches / worktrees)
├── github.rs        GitHub integration (via gh CLI — PR/Issue/diffs, async)
├── diff.rs          Unified diff parser + diff view (side-by-side/inline, word-level)
├── ide.rs           Hand-off to external IDEs (open at the current line)
├── panels.rs        Rendering for the GitHub panel, PR diff tabs, IDE integration
├── kanban.rs        Fleet board (8 state lanes, card actions, live terminal pane)
├── race.rs          Prompt fan-out race (parallel worktrees, adopt/discard, collision detection)
├── instances.rs     Registry of running instances (the detection behind zai status)
├── approvals.rs     Unified approval queue (classification, policy resolution, audit log)
├── tutorial.rs      First-run guided tour (26 steps, highlights each target in place)
├── supervisor.rs    Agent supervision (stall/loop/abnormal-exit detection and notification)
├── coordinator.rs   Inter-agent messaging and task reassignment
├── orchestration.rs Task creation UI, hand-off driving, message send/receive assembly
├── commander.rs     Commander (named Super Agent) directive parsing — `@target: instruction` → 📮 notification
├── diagnostician.rs Legacy LLM diagnosis (no longer spawned; used to identify the commander session and for UI display)
├── markdown.rs      Markdown parsing and preview rendering
├── html.rs          HTML preview rendering
├── jsonc.rs         Reading JSON with comments (JSONC)
├── cli.rs           `zai` subcommands (the control channel for driving the app from outside)
├── lsp.rs           LSP client (diagnostics, completion, hover, references, rename, format, symbols)
├── terminal.rs      PTY sessions + vt100 rendering + approval-prompt detection/auto-reply
├── shellenv.rs      PATH resolution for child processes + OS-independent `which`
├── agents.rs        Session management (launch/restart/destroy/broadcast/permission modes)
├── mcp.rs           MCP server config discovery/parse/enable-disable (env values never held)
├── skills.rs        Skills / slash command discovery and parsing (read-only, three tiers)
├── failover.rs      Account failover on rate limits (off by default, evidence rung shown)
├── remote.rs        Phone remote (built-in HTTP server, QR code, token auth)
├── tunnel.rs        SSH reverse tunnel (works off-LAN; binds loopback only while active)
├── firewall.rs      Windows inbound rule (what 📱 needs: check / allow / revoke)
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
- [x] Diff view at VS Code parity (side-by-side ⇔ inline, word-level highlighting, F7 change jumps, context folding, syntax colours)
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
- [x] Super Agent redesigned as the Commander (name any running agent; `@target: instruction` goes through you as a 📮 notification — no auto-injection into input boxes)
- [x] Auto-YES unified into one switch (`pet_auto_yes`, off by default = you approve, independent of launch mode, one answer per prompt)
- [x] Editor + Cockpit split view (read the code while commanding)
- [x] Chat history saved per folder and resumed (scrollback replay + claude `--continue` / codex `resume --last`)
- [x] Clipboard image paste (⌘V / Ctrl+V saves a PNG and inserts `@path`)
- [x] Image viewer (png/jpg/gif/webp/ico, zoom & fit, transparency checkerboard) and read-only PDF text
- [x] Word-wrap / whitespace toggles (`word_wrap` / `show_whitespace`)
- [x] Git status in the file tree (M/A/U/D/R/C, tinted ancestors) and follow-the-active-file
- [x] Fleet board rebuilt (8 lanes, vertical mode, live terminal on selection, facts marked apart from guesses)
- [x] Prompt fan-out race (one prompt across N agents, adopt/discard, collisions caught while racing)
- [x] Inline review comments on diffs (resolve tracking + one assembled prompt)
- [x] Running-instance detection (`zai status` / `zai status --json`, branch on the exit code)
- [x] Reopen the previous folder on launch (`--no-restore` / `ZAIVERN_NO_RESTORE` to disable)
- [x] Paint-panic quarantine with self-recovery + the Windows freeze fixed (ConPTY resize coalescing, off-thread file dialogs)
- [x] CI moved to 6 parallel jobs (3 OSes × fast/pty) on cargo-nextest with a `ci` profile
- [x] LSP up to VS Code parity (completion, hover, references, rename, format, symbols, format-on-save)
- [x] Project-wide search upgrades — regex, globs, whole-word, dry-run bulk replace (⇧⌘H)
- [x] Cross-agent usage aggregation and exhaustion forecasting (status bar + detail panel)
- [x] Listing and resuming past sessions (pick one from the sidebar, resume in that folder)
- [x] Line-ending detection/conversion + save-time cleanup (trailing whitespace, final newline)
- [x] Folding, guides, sticky headers, bookmarks, reopen-closed-tab, multiple carets / column selection
- [x] CSV / TSV table view and large-file mode + reopen and save with a chosen encoding
- [x] VS Code-compatible snippets + Emmet (`~/.zaivern/snippets/`), and a real-document Markdown preview (GFM, raw HTML, images) + Mermaid diagrams and TeX math
- [x] PR-style local review panel (compare-target switching, stage/discard, deep git tree coloring)
- [x] Unified approval queue (9 kinds, 4 scope levels, audit log, privilege always manual)
- [x] First-run guided tour (26 steps), a per-agent multiline composer, auto-answered numbered menus / surveys
- [x] Discoverable as "Zaivern" in the OS process list (all 3 OSes) + 0.13% idle CPU
- [x] `zai status --pid-only`
- [x] Vertical agent deck (cmux-style, running agents only, `⌘⇧L`) + terminal splits
- [x] Branch switching from the toolbar (refuses with a reason on in-progress operations, branches held by another worktree, or uncommitted changes — never uses `git stash`)
- [x] Repository-wide session list (folds by branch across worktrees)
- [x] Command palette reorganized (8 groups, best-match ordering, recents)
- [x] Split editor (right / down / nested, shared buffers, per-pane view state, layout restored across restarts)
- [x] Every file opens (hex viewer, video/audio info cards, ZIP/JAR/WHL listing, 40+ magic-number formats)
- [x] Minimap (search-hit / diagnostic / bookmark marks) and breadcrumbs (path part without an LSP, middle elided)
- [x] The rest of LSP (quick fix ⌘., signature help ⇧⌘Space, document highlight, format selection)
- [x] Git blame (visible range only, fetched async; click for that commit's diff) and drag-to-reorder tabs
- [x] `.vscode/tasks.json` support (JSONC; a task with an unsupported variable is greyed out with the reason, not dropped)
- [x] MCP server manager (six config files in one table, `env` values never displayed)
- [x] Skills / slash command manager (project / user / plugin tiers)
- [x] Account failover on rate limits (off by default, evidence rung and "estimate" spelled out)
- [x] SSH reverse tunnel (phone works off-LAN; binds 127.0.0.1 only while active)
- [x] Headless `zai` (`worktree` / `session` / `agent`, `--json`, exit codes 0/1/2)
- [x] Keybinding reachability pinned by tests + on-screen shortcut labels generated from the keybinding table
- [x] `cargo fmt --all --check` and a clippy ratchet in CI (existing debt frozen, new debt blocked)
- [ ] Plugin grammars (TextMate) & registry sharing

## License
Apache License 2.0 — see [LICENSE](LICENSE) for details.

---

<div align="center">

**The agents are already fast enough.**<br>
**The next thing to get faster is you — the one in command.**

</div>
