<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

**把 Claude Code、Codex、Gemini CLI 以及你已经在用的其他 AI 编程 CLI，收进同一个驾驶舱。**<br>
在 macOS、Windows、Linux 上，用一个原生应用启动它们、盯住它们、指挥它们。

[English](README.md) | [日本語](README.ja.md) | **简体中文** | [한국어](README.ko.md) | [Português (Brasil)](README.pt-BR.md) | [Español](README.es.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

[**下载**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**快速开始**](#快速开始) ·
[**文档**](#文档) ·
[**官网**](https://zaivern.com/)

<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Zaivern Code 并排运行 Claude Code、Codex、Gemini CLI 等编程智能体" />
</a>

<!-- 出典: docs/conflict-zero.md §3.12 — zaivern-code / 書き手 16 / zai 0.14.0:
     素の git 26 ファイル・28 ハンク、zaivern あり 0/0・96/96 成立・拒否 0・30 件ずらし -->
**16 个智能体同时向这个仓库写入** —— 纯 git：**26 个文件冲突 / 28 个冲突块**。<br>
接入租约台账后：**0 / 0**，而且 **96 处编辑全部落地** —— 没有一处被拒绝，其中 30 处被挪到了空闲的行域。<br>
[查看实测数据 →](docs/conflict-zero.md)

如果 Zaivern Code 对你有用，一颗 ⭐ **Star** 就是对它开发的支持。

</div>

## 为什么需要 Zaivern Code

启动好几个 AI 编程 CLI 很容易，难的是持续掌握它们。每个智能体都待在自己的终端标签页里，
按自己的节奏索要审批，并且在不知道其他智能体在做什么的情况下改动文件。

<!-- 出典: docs/conflict-zero.md §3.3 — 書き手 64 / 重なり 0.5:
     ベースラインは 57/64 のマージが衝突し 132 ハンク、ガード側は全規模で 0 ハンク -->

| 没有驾驶舱 | 有 Zaivern Code |
|---|---|
| 并行的智能体越多，合并冲突越多 | 共享台账让智能体互不踩线 —— 64 个智能体时冲突块为 0，而纯 git 产生了 132 个 |
| 轮流切标签页，找谁在等你 | 所有智能体在同一屏，附带实时状态 |
| 同一条指令往每个工具里粘一遍 | 一次广播给整支队伍，或只指定某一个智能体 |
| 漏看一次审批提示，整轮运行就白跑 | 通知 + 一键审批 |
| 智能体干活时你只能守在桌前 | 用手机查看进度并审批 |

Zaivern Code 不是 AI 模型，也不捆绑任何模型。它驱动的是你已经安装并登录好的 CLI ——
有一个就够开始了。

## 快速开始

**前置条件。** 安装并登录至少一个受支持的 AI 编程 CLI。Zaivern Code 内置了 33 种启动预设，
其中包括 Claude Code、Codex 和 Gemini CLI。你不需要准备一个以上。

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

两个安装脚本都会**在解包之前**用发布的 `checksums.txt` 校验发行包。只要 SHA-256 对不上 ——
或者根本取不到校验和 —— 就直接中止，不解包、也不运行任何东西。

不想把脚本管道送进 shell？可以从
[Releases](https://github.com/tacyan/zaivern-code/releases/latest) 下载对应平台的压缩包，
解开后把 `zai`（Windows 上是 `zai.exe`）放到 `PATH` 里的任意位置，然后在项目文件夹中执行
`zai .`。手工校验下载、检查构建来源（provenance）、阅读 SBOM 的方法见
[SECURITY.md](SECURITY.md)。

窗口打开之后：

1. 点击 `+ Agent`，选一个你已经装好的 CLI。
2. 在输入框里写下任务并发送。
3. 等第一个用顺手了，再加第二个智能体。

### 更新

```bash
zai update            # 检查是否有新版本，显示将要执行的命令，然后升级
zai update --check    # 只查看，不做任何改动
zai update --yes      # 跳过确认提示直接升级
```

无论编辑器是否正在运行，`zai update` 都能用，它会通过对应平台的安装脚本就地升级。
重新执行上面那条一行命令，效果相同。

`zai uninstall` 用于卸载（`--dry-run` 会列出将被删除的内容）。卸载只会动可执行文件本体和
`~/.zaivern`；`PATH` 上的其他内容只会被列出来，绝不会被删除。

## 界面语言

内置 **English、日本語、简体中文、한국어、Português (Brasil)、Español** 六种语言，
切换**不需要重启**，选中之后的下一帧整个界面就换好了。

入口有：工具栏的 🌐 菜单、菜单栏的「视图 → 界面语言」、命令面板，以及设置里的「外观」。
无论从哪里改，最后都会落到 `~/.zaivern/config.toml` 的 `ui_language`；默认值 `"auto"`
跟随操作系统的语言。

没有内置的语言可以直接从 GitHub 装，不用重新构建应用：

```sh
zai lang list --remote        # 查看发布源里有哪些语言包
zai lang install zh-CN
zai lang set zh-CN
zai lang install fr --from someone/zaivern-lang-fr
```

想自己做一份：`zai lang export fr` 会把模板写到 `~/.zaivern/locales/fr.json`，编辑之后用
`zai lang check fr` 检查缺漏，再 `zai lang set fr` 生效。**只要写一条和内置相同的 ID，
就只覆盖那一条**，为了改一个词而抄一整本词典是不必要的。

细节见 [docs/i18n.md](docs/i18n.md)。

## 主要功能

### 冲突协调（这个项目存在的理由）

智能体会在按仓库划分的共享台账里，先认领它即将编辑的文件 —— 或者具体的行域 ——
而 git 钩子会拒绝任何会撞车的写入。

<!-- 出典: docs/conflict-zero.md §3.8.1 — --layout disjoint / 64 体:
     B (ファイル単位の所有) 完了 1・拒否 63、Cref (行域) 完了 64・拒否 0・ハンク 0 -->
真正让它能扛住规模的是行域。把 64 个智能体指向同一个文件，文件级租约只会放行其中 **1** 个，
拒绝另外 **63** 个；换成行域所有权，**64** 个全部通过、没有一个被拒绝，而合并结果依然是
**0** 个冲突块。

<!-- 出典: docs/conflict-zero.md §3.12.2 — 錨の誤マッチによる二重配布と、その修正 -->
行域是靠锚点追踪的 —— 也就是它首行和末行的内容 —— 而不是靠行号，所以在它上方发生编辑时依然
成立。如果重新解析锚点后落到了台账记录之外的位置，这次读数会被丢弃而不是被采信，因此一次
认领绝不会悄悄迁移到文件的另一处。

这些都拦不住语义冲突；[下面的章节](#冲突协调)会写清楚哪些在覆盖范围内、哪些不在。

### Agent Cockpit

把多个 AI CLI 平铺并排，一眼看出哪个在思考、在编辑、在运行，或者在等你。内置了 33 种工具的
启动预设，所以添加一个智能体是两次点击的事，而不是去回忆一条命令行。

### 广播

从一个输入框把同一条指令发给所有正在运行的智能体，或者只挑一个智能体来做定向控制。
当同一条修正适用于整支队伍时特别好用。

### 状态、审批与通知

Zaivern Code 会把权限提示、停滞和意外退出都变成通知，让你一键处理。自动审批默认关闭，
必须由你主动打开。

### 手机远程

用手机查看进度、下达指令、批准操作、编辑文件。最简单的方式是在同一个 Wi-Fi 网络下使用。
不在同一网络时，有两种传输方式可以接手：**[Tailscale](https://tailscale.com/)**（前提是两台
设备已经在同一个 tailnet 里），或者经由一台你本来就能连上的主机建立 SSH 隧道。切换传输方式
只会改变服务端监听的位置 —— 令牌、端口和页面都不变，所以手机上已经扫过的二维码可以继续用。

Tailscale 模式不需要跳板机，也不需要端口转发：在电脑和手机上都装好
[Tailscale](https://tailscale.com/download)，登录同一个 tailnet，然后在手机远程窗口（📱）里
点 **🔒 Listen on Tailscale**。它只绑定 tailnet 地址和 `127.0.0.1`，别的一概不绑，所以你当下
连着的咖啡馆或机场 Wi-Fi 根本看不到这个端口。Zaivern 从内核路由表里取 tailnet 地址，从不去调用
`tailscale` 命令 —— 在 macOS 上那个 CLI 是个 shell 包装器，守护进程连不上时可能永远不返回，
而挂住的子进程会冻住 UI。

### 内置编辑器

不离开应用就能读代码、审阅智能体改了什么，包括图片、PDF、CSV 和 Markdown。未保存的缓冲区能
挺过一次崩溃：下次启动会恢复它们；如果这期间磁盘上的文件变了，会把差异摆给你看，而不是默默覆盖。

## 冲突协调

台账里的认领不是建议：钩子会在撞车的写入被尝试的那一刻就拒绝它，于是冲突在那里浮出水面，
而不是拖到合并时才发现。

<!-- 出典: docs/conflict-zero.md §3.16.6 — dup_lines=0 は常に成立 (内容に依存しない)、
     conflict_files=0 は条件付き (帯 + 壁 + 昇順。反復的な内容では断ることがある) -->
有两条保证，成立的程度并不相同，把它们混为一谈就会夸大其词。「不会把同一批行同时发给两个
智能体」是台账本身的性质，无论文件内容是什么都成立。「随后合并能一次通过」则是有条件的：
它需要安全带、两个行域之间有一行唯一的内容（墙），以及升序排列。重复性强的内容会破坏后者，
而前者依然成立；这种情况下闸门会选择拒绝，而不是靠猜。

它拦不住的是语义冲突：一个智能体改了函数签名，另一个在别的文件里照旧调用旧签名，
合并干干净净。

```console
$ zai czero init      # 安装台账、git 钩子和合并驱动，然后自检
$ zai czero verify    # 在一个用完即弃的仓库里制造真实冲突，确认它会被拦住
```

适用范围、限制，以及支撑它们的实测数据都在
[docs/conflict-zero.md](docs/conflict-zero.md)。

## 资源占用

<!-- 出典: docs/idle-cost.md §7 — 2026-08-15、同一マシン・同一セッションで
     Zed 1.15.0 / zai 0.16.0 / zai 0.17.0 を交互に 3 ラウンド、9/9 VALID。
     0.16.0 を陽性対照に入れてあるので「測定が生きていること」まで示せる。
     0.17.0 は測定床に張り付いているので必ず「≤」で書くこと -->

一个开一整天的编辑器，在你不打字的时候就该什么都不花。在同一台机器、同一次会话中，
在两个应用之间来回切换三轮测得（macOS 26.5.2、接电源、180 秒观测窗口、只有 4 个文件的中立工作区）：

| | Zed 1.15.0 | Zaivern Code 0.17.0 |
|---|---:|---:|
| 空闲 CPU（3 次中位数） | 单核的 0.761% | **≤0.006%** —— 已到测量下限 |
| 下载体积 | 424.6 MB（`.app`） | **28.7 MB**（单个二进制） |
| RSS | 162.2 MB | 170.3 MB |

这张表刻意谨慎的地方有两处：

- **`≤0.006%` 是下限，不是读数。** `ps` 只把 CPU 时间分辨到 1/100 秒，所以 180 秒的窗口
  区分不出低于 0.006% 的任何值。三轮都恰好落在一个刻度上。诚实的说法是「至少比 Zed 低 127 倍」，
  而不是一个具体的比值。
- **RSS 不是胜利，我们也不声称是。** Zed 同样是用 Rust 写的；两者相差在 5% 以内，属于噪声。
  真正差出一个数量级的是下载体积。

同一次运行里，Zaivern Code **0.16.0** 测得 8.933% —— 这是阳性对照。如果同一次会话中没有一个
会给出高读数的版本，那么一个接近零的结果就无法与「测量坏掉了」区分开。0.17.0 空闲开销下降，
是因为引导教程不再无条件预留帧，并且每两秒一次的清理重绘被移除了。

复现方式：`tools/idle-duel.sh --vs Zed --out /tmp/duel.tsv`。这套测量装置在无法诚实测量时
会拒绝测量：它按 pid 核实应用位于最前台，要求机器全程无人操作，并把证据记进每一行。
完整方法、原始数字，以及我们踩过的坑都在 [docs/idle-cost.md](docs/idle-cost.md)。

## 支持的平台

| 项目 | 支持范围 |
|---|---|
| OS | macOS arm64/x86_64、Linux x86_64/arm64、Windows x86_64 |
| AI CLI | 33 种启动预设，包括 Claude Code、Codex 和 Gemini CLI |
| Rust | 1.88+ —— 仅在从源码构建时需要 |
| 许可证 | Apache-2.0 |

常见的一种配置是 Claude Code 负责实现、Codex 负责测试、Gemini CLI 负责写文档，
但 Zaivern Code 并不预设这种分工。任意组合都可以，只用一个智能体也可以。

## 安全性

- 默认为需要审批的模式；Auto-YES 需要按会话主动开启。
- 权限提升始终需要手动审批。
- MCP 的环境变量值从不显示 —— 只显示是否已设置。
- 会话被销毁或应用退出时会停止子进程，不会留下孤儿智能体在后台继续跑。

## 常见问题

**这和用 tmux 分屏有什么不同？**

tmux 只是把终端平铺，它并不知道里面在跑什么。Zaivern Code 会读取每个智能体的状态，
所以它能显示哪个在思考、在编辑，或者卡在审批提示上，并把那个提示变成你一键即可回复的通知。
tmux 完全没有对应物的部分是共享台账：两个智能体在物理上写不到同一批行，因为撞车的第二次写入
会在被尝试的那一刻就被 git 钩子拒绝，而不是留到合并时才被发现。

**租约台账会拖慢速度吗？**

<!-- 出典: docs/conflict-zero.md §1「意味しないこと」4 / §3.3 (掃引: 4〜8 体 p50 40〜50ms、
     64 体 p50 160ms、busy-deny 32 体 4 件・64 体 14 件) / §3.4 (ゲート 1536 回で p50 298.7ms)。
     体数だけでは決まらないので、必ず担当表の大きさを添えること -->
会，而且规模越大越明显，因为闸门就坐在写入路径上。在标准扫描下 —— N 个写入方、N×6 个文件 ——
4～8 个智能体时闸门延迟为 p50 40–50 ms，64 个时为 p50 160 ms。智能体数量不是唯一变量：
一张更重的分配表若把闸门调用 1536 次，同样是 64 个智能体也会达到 p50 298.7 ms，所以任何
「64 个智能体时开销是 X」的单一数字，不附上工作负载的规模就是不完整的。从 32 个智能体开始，
闸门在来不及判定时还会回答 `busy-deny`：它选择拒绝而不是猜测，重试就能通过，但在你看来
就是偶尔会被拒。只有一两个智能体时，闸门不在你的关键路径上。

**「零冲突」到底是什么意思？**

它比字面听上去要窄，而且是刻意如此：

<!-- 出典: docs/conflict-zero.md §3.2 (書き手 8 / 重なり 1.00: 10/48 成立・38 件をゲートが停止)、
     §3.8.1 (disjoint / 64 体: 素の git のハンクは全規模 0。B は完了 1、Cref は 64)、§3.16.6 -->
- **零是靠拒绝写入换来的。** 八个写入方全部瞄准同一批文件时，计划中的 48 处编辑只写成了 10 处，
  另外 38 处被闸门拦下。冲突数是 0；吞吐量不是。
- **相隔足够远的行域本来就不需要帮助。** 纯 git 已经能零冲突地合并它们。行域所有权并不是在做
  git 做不到的事 —— 它只是把文件级租约摧毁掉的那部分并行度还回来（64 个里通过 1 个，
  对比 64 个里通过 64 个）。
- **两条保证强度不同。** 「不会把同一批行发给两个智能体」永远成立；「合并一次通过」是有条件的，
  在重复性强的内容上会失效。

[docs/conflict-zero.md](docs/conflict-zero.md) 开篇就是这条边界，并且承载了它背后的每一项实测，
包括那些后来被推翻的主张。

## 文档

| 文档 | 内容 |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | 「无冲突」声称了什么、没有声称什么，以及背后的实测 |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | 哪种仓库形态适用哪些保证 |
| [docs/plugins.md](docs/plugins.md) | 如何编写插件，附[格式规范](docs/PLUGIN_SPEC.md) |
| [docs/README.md](docs/README.md) | 其余所有文档的索引，按其支撑的主张分组 |

各版本的发布说明在
[Releases 页面](https://github.com/tacyan/zaivern-code/releases)。

## 参与贡献

欢迎提交缺陷报告、功能建议和 Pull Request。新建之前请先在
[Issues](https://github.com/tacyan/zaivern-code/issues) 里确认是否已有相同报告，
并把 [Pull Request](https://github.com/tacyan/zaivern-code/pulls) 提向 `main`。

```bash
git clone https://github.com/tacyan/zaivern-code.git
cd zaivern-code
rustup update stable
cargo run --release -- .
```

[CONTRIBUTING.md](CONTRIBUTING.md) 涵盖了其余内容：如何验证一处改动、如何在本地跑 Linux 和
Windows 的检查，以及这个仓库遵循的约定。

## 许可证

[Apache License 2.0](LICENSE)

---

<div align="center">

**智能体已经够快了。接下来该轮到你指挥得更快。**

</div>
