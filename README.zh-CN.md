<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

### 64 个 AI 智能体。一个仓库。零合并冲突。

**并行编码智能体的协调层。**

让 Claude Code、Codex、Gemini CLI 等编码智能体在同一个仓库上运行 —— 不再被合并冲突拖垮。

[English](README.md) | [日本語](README.ja.md) | **简体中文** | [한국어](README.ko.md) | [Português (Brasil)](README.pt-BR.md) | [Español](README.es.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

<!-- TODO: 换成 15-20 秒的基准演示:
     一个仓库 64 个智能体 / 原生 git 132 个冲突块 / Zaivern 0 个。
     下面这段 GIF 展示的是驾驶舱，不是协调结果。 -->
<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Zaivern Code 并排运行 Claude Code、Codex、Gemini CLI 等编码智能体" />
</a>

| 64 个写入者 · 同一仓库 · 同样的工作量 | 原生 git | Zaivern Code |
|---|---:|---:|
| 发生冲突的合并 | 64 次中 57 次 | **0** |
| 冲突块 | 132 | **0** |

[查看测量方法、代价与边界 →](docs/conflict-zero.md)

[**快速开始**](#快速开始) ·
[**实测数据**](#实测数据) ·
[**文档**](#文档) ·
[**下载**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**官网**](https://zaivern.com/)

</div>

## 问题

跑一个编码智能体很容易，跑四个就不是了。两个智能体改同一个文件，问题就已经出现：

- 它们改到同一批行，而你在合并时才发现。
- 看不出哪个在干活、哪个被卡住、哪个已经悄悄停了。
- 审批提示在你没看的标签页里滚了过去。
- 集成变成了你的工作 —— 每一次都是。

瓶颈不是智能体本身，而是**它们之间的协调**。

## 解决方式

Zaivern Code 负责协调每个编码智能体可以安全编辑仓库的哪些部分。
它不是等到合并时才发现冲突，而是在**冲突的写入落地之前**就拦住重叠的工作，
同时把“查看、操控、恢复”正在运行的智能体这件事收拢到一个地方。

```text
没有 Zaivern                              有 Zaivern

智能体 1  ─┐                              智能体 1  ─┐
智能体 2  ─┤                              智能体 2  ─┤   ┌─────────────┐
智能体 3  ─┼─→ 同一批文件 ─→ 合并冲突      智能体 3  ─┼─→ │  行区间账本  │ ─→ 干净地
   ...    ─┤                                 ...    ─┤   │             │    完成集成
智能体 64 ─┘                              智能体 64 ─┘   └─────────────┘

132 个冲突块                              0 个冲突块
```

**不需要 64 个智能体也会遇到。** 两个智能体改同一个文件就够了。从 2 个开始，扩到 64 个。

## 快速开始

先安装并登录至少一个受支持的 AI 编码 CLI —— Zaivern Code 内置 **33 个**启动预设，
开始时有一个就够。

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

然后在窗口里点 `+ Agent`，选一个你已经装好的 CLI，给它派任务。

为某个仓库开启冲突协调：

```bash
zai czero init      # 安装账本、git 钩子与合并驱动，然后自检
zai czero verify    # 在一次性仓库里造一个真实冲突，确认它会被拦住
```

安装脚本会在**解压之前**用发布的 `checksums.txt` 校验压缩包，不匹配就中止。
[手动下载、校验、构建溯源与 SBOM →](SECURITY.md)

### 更新

```bash
zai update            # 检查新版本，显示命令，然后升级
zai update --check    # 只看不改
zai update --yes      # 跳过确认直接升级
```

无论编辑器是否在运行都能用。卸载用 `zai uninstall`。

## 核心功能

### 1. 并行运行智能体，而不陷入合并冲突

智能体在动手前会认领文件或行区间。如果另一个在运行的智能体已经拥有那片区域，
git 钩子就会拒绝这次会撞车的写入 —— 在写入的那一刻，而不是合并的时候。

在行区间互不相交的 64 智能体基准里，**64 个**全部落地，冲突块为 **0**；
换成文件级租约，则只会放行 1 个。
[行区间协调的原理 →](docs/conflict-zero.md)

### 2. 并行智能体管理

把多个 AI CLI 并排铺开，一眼看出哪个在思考、在编辑、在运行、在等你。
新增一个智能体是两次点击，而不是回忆命令行。

### 3. 健康状态与停滞检测

Zaivern 看的是**语义上的进展**，不是屏幕像素：不再产生进展的智能体会被报告为**停滞**，
意外退出会变成通知。

### 4. 群发指令

从一个输入框把同一条指令发给所有运行中的智能体，也可以只针对其中一个。

### 5. 审批

默认需要人工审批。自动 YES 需按会话显式开启，权限提升始终由人确认，
MCP 的环境变量不显示值。

### 6. 手机遥控

在手机上查看进度、下达指令、批准操作、编辑文件。可用同一个 Wi-Fi、
[Tailscale](https://tailscale.com/) 或 SSH 隧道。

### 7. 内置编辑器

不离开 Zaivern 就能审阅代码和智能体的改动，包括 Markdown、图片、PDF 和 CSV。
未保存的缓冲区会在崩溃后恢复。

此外还有插件机制，以及六种语言的界面。
[插件文档](docs/plugins.md) · [翻译文档](docs/translating.md)

## 工作原理

1. **启动** —— 在一个窗口里拉起编码智能体，或接管已经在跑的。
2. **认领** —— 编辑之前认领文件或行区间，并锚定到周围的内容。
3. **闸门** —— git 钩子在重叠的写入抵达合并之前就拒绝它。
4. **集成** —— 不重叠的改动照常由 git 合并。

[技术细节 →](docs/conflict-zero.md) ·
[哪些保证在哪种仓库形态下成立 →](docs/czero-repo-shapes.md)

## 支持的智能体

Claude Code · Codex · Gemini CLI · Cursor Agent · GitHub Copilot CLI ·
**另有 28 个** —— 共 33 个启动预设，另外还有 6 个可通过 ACP 驱动。

Zaivern Code 不是 AI 模型，也不捆绑模型：它驱动的是你已经安装并登录好的 CLI。
任意组合都可以，只用一个也行。没有你在用的那个？
[提一个接入需求](https://github.com/tacyan/zaivern-code/issues)。

## 为什么选 Zaivern

|  | 终端复用器 | 通用智能体面板 | Zaivern Code |
|---|:---:|:---:|:---:|
| 同时运行多个智能体 | ✅ | ✅ | ✅ |
| 一块屏幕看全部 | ❌ | ✅ | ✅ |
| 知道状态（思考 / 阻塞 / 停滞） | ❌ | 不一定 | ✅ |
| 行区间所有权 + 写入时拒绝 | ❌ | ❌ | ✅ |
| 审批以通知形式出现 | ❌ | 不一定 | ✅ |
| 手机 / 远程操作 | ❌ | 不一定 | ✅ |
| 单个原生可执行文件，无运行时 | 不一定 | 不一定 | ✅ |

## 实测数据

**64 个智能体、同一仓库、同样的工作量**（文件数 = 写入者 × 6，文件重叠 50%）：

| | 原生 git | Zaivern Code |
|---|---:|---:|
| 发生冲突的合并 | 64 次中 57 次 | **0** |
| 冲突块 | 132 | **0** |

这个零是靠拒绝写入换来的：计划中的 384 次编辑落地了 202 次，其余被闸门拦下。
当行区间本来就互不相交时，64 个智能体全部落地，一次拒绝也没有。

**在这个仓库自身上跑 16 个智能体**（zai 0.14.0）：原生 git 产生了
**26 个冲突文件 / 28 个冲突块**；接入账本后是 **0 / 0**，并且 **96 次编辑全部落地** ——
零拒绝，其中 30 次被挪到空闲的行区间。

### “零冲突”意味着什么

- Zaivern 可能会**拒绝**重叠的写入，而不是让它变成一次合并冲突。冲突数是 0，吞吐量不是。
- 它防止的是行所有权的重叠。它**不检测语义冲突** —— 一个智能体改了函数签名、
  另一个还在用旧调用，合并依然干干净净。
- 距离足够远的行区间本来就不需要帮忙：原生 git 已经能零冲突地合并它们。
  行区间所有权只是把文件级租约毁掉的并行度还了回来。

[完整方法、各规模数据、闸门延迟与边界 →](docs/conflict-zero.md)

## 支持的平台

| 项目 | 支持情况 |
|---|---|
| 操作系统 | macOS arm64/x86_64、Linux x86_64/arm64、Windows x86_64 |
| AI CLI | 33 个启动预设，另有 6 个通过 ACP |
| 测试 | 4,985 个，在 CI 的 macOS、Linux、Windows 上运行 |
| 许可证 | Apache-2.0 |

## 文档

| 文档 | 内容 |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | “零冲突”主张什么、不主张什么，以及背后的每一次测量 |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | 哪些保证在哪种仓库形态下成立 |
| [docs/plugins.md](docs/plugins.md) | 编写插件，附[格式规范](docs/PLUGIN_SPEC.md) |
| [docs/README.md](docs/README.md) | 其余全部文档的索引，按其支撑的主张分组 |

[空闲 CPU 与二进制体积的实测 →](docs/idle-cost.md) ·
[发布说明](https://github.com/tacyan/zaivern-code/releases)

## 试一试

如果并行编码智能体是你日常工作的一部分，就在下一个多智能体任务上跑一次 Zaivern Code ——
在仓库里执行 `zai czero init`，让两个智能体指向同一个文件，
看着第二次写入**被拒绝，而不是被糟糕地合并**。

## 社区

- 发现了协调上的边界情况？[提 issue](https://github.com/tacyan/zaivern-code/issues)。
- 在用还没支持的编码智能体？[提接入需求](https://github.com/tacyan/zaivern-code/issues)。
- 在跑 8、16、32 或 64 个智能体？分享你的实测 —— `tools/conflict-bench.sh` 和
  `tools/anyrepo-prove.sh` 会产出与上表可比的数字。
- 用 Zaivern Code 做了什么？把你的配置展示出来。

欢迎向 `main` 提 Pull Request —— 从源码构建（Rust 1.88+）、验证改动、
在本地跑 Linux 与 Windows 检查的方法见 [CONTRIBUTING.md](CONTRIBUTING.md)。

如果 Zaivern Code 对你有用，一个 ⭐ **Star** 能帮别人找到它。

## 许可证

[Apache License 2.0](LICENSE)
