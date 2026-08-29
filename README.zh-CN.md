<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

### 同时运行多个编程智能体，不再陷入合并冲突。

**从 2 个智能体开始，扩展到 64 个。**
Zaivern Code 在重叠的改动落地之前就拦住它们，因此它们不会变成合并冲突。

一个窗口容纳 Claude Code、Codex、Gemini CLI 以及你已经装好的另外 30 种智能体 CLI。
单个原生二进制 —— macOS、Linux、Windows。

[English](README.md) | [日本語](README.ja.md) | **简体中文** | [한국어](README.ko.md) | [Português (Brasil)](README.pt-BR.md) | [Español](README.es.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

</div>

**安装并启动**

macOS / Linux：

```bash
curl -fsSL https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.sh | sh
zai .
```

Windows PowerShell：

```powershell
irm https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.ps1 | iex
zai .
```

需要你至少已经安装并登录一种受支持的 AI 编程 CLI。
Zaivern Code 只是驱动你现有的 CLI，本身不附带任何 AI 模型或订阅。

**冲突协调（可选）：**

```bash
zai czero init
```

这会修改当前的 Git 仓库。
[预览并验证这些改动 →](#启用冲突协调) ·
[手动下载与校验](SECURITY.md)

<div align="center">

<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Zaivern Code 驾驶舱：多个编程智能体 CLI 并排显示在同一个窗口中，并标出各自的状态" />
</a>

[**快速开始**](#快速开始) ·
[**实测数据**](#实测数据与限制) ·
[**文档**](#文档) ·
[**下载**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**官网**](https://zaivern.com/)

</div>

*上面的动图是驾驶舱 —— 多个智能体 CLI 在同一个窗口里。它并没有展示冲突协调的结果，
那部分是单独测量的，就在下面。*

## 实证

**64 个智能体、一个仓库、同一份工作量。** 文件数 = 写入者 × 6，其中一半会被
不止一个智能体盯上。同一份任务清单跑两遍：一遍走原生 git，一遍走 Zaivern Code 的行区间账本。

| | 原生 git | Zaivern Code |
|---|---:|---:|
| 发生冲突的合并 | 64 次中 57 次 | **64 次中 0 次** |
| 需要人来解决的冲突块 | 132 | **0** |
| 成功落地的改动 | 384 中 384 | 384 中 202 |
| 落地前被拦下的写入 | 0 | 182 |

**这个零是用「拒绝写入」换来的，而不是把两边变魔术般合到一起。**
计划中的 384 次改动里有 182 次在关卡处被拦下，因为那些行已经属于另一个在跑的智能体；
其中 14 次是拥塞退避，重试后有可能通过。

**当行区间真正互不相交时，一次也不会被拒。** 64 个智能体编辑**同一个**文件的
64 个独立行区间，**64 次全部落地**，拒绝 **0** 次，冲突块 **0** 个 ——
而按文件加锁只会放行 1 个、拒绝 63 个。

语义冲突**不在检测范围内**：一个智能体改了函数签名、另一个仍按旧写法调用，
两次写入都会放行，git 也会干净地合并。

[测量方法、各规模数据、关卡延迟以及所有尚未填上的坑 →](docs/conflict-zero.md)

## 问题

跑一个编程智能体很容易，跑四个就不是了。**两个智能体改同一个文件就已经够呛：**

- 它们改到同一行，而你在合并时才发现。
- 你看不出哪个在干活、哪个卡住了、哪个悄悄停了。
- 审批提示在你没看的那个标签页里滚了过去。
- 集成变成了你的工作 —— 每一次都是。

瓶颈不是智能体，而是**它们之间的协调**。

## 解决方式

Zaivern Code 负责协调每个智能体可以安全编辑仓库的哪些部分。
它不是在合并时才发现碰撞，而是**在冲突的写入落地之前**就抓住重叠的工作，
并且把观察、指挥和恢复这些智能体的地方收拢到一处。

```text
没有 Zaivern                             有 Zaivern

Agent 1  ─┐                              Agent 1  ─┐
Agent 2  ─┤                              Agent 2  ─┤   ┌─────────────┐
Agent 3  ─┼─→ 同一批文件 ─→ 合并冲突      Agent 3  ─┼─→ │  行区间的   │ ─→ 干净
   ...   ─┤                                 ...   ─┤   │    账本     │    集成
Agent 64 ─┘                              Agent 64 ─┘   └─────────────┘
```

## 快速开始

### 启动多智能体驾驶舱

用本页顶部的一行命令安装，然后在项目目录里运行 `zai .`。
它会在该目录打开驾驶舱 —— 智能体窗格、编辑器、手机遥控。
点 `+ Agent`，选一个你已装好的 CLI，把任务交给它。
**这一步并不会开启冲突协调**，那是下一步的事。

安装脚本会**在解包之前**把下载到的压缩包和该版本的 `checksums.txt` 比对，
不一致就中止。
[手动下载、校验和验证、构建来源与 SBOM →](SECURITY.md)

### 启用冲突协调

```bash
zai czero init --dry-run  # 预览将要发生的改动
zai czero init            # 安装账本与 Git 集成
zai czero verify          # 在一次性仓库里验证
zai .                     # 启动驾驶舱
```

- **`zai czero init --dry-run`** 只预览将要发生的改动，不会修改当前仓库。
- **`zai czero init` 会修改当前的 Git 仓库。** 它会建立行区间账本、加入
  `pre-commit` / `pre-applypatch` / `pre-merge-commit` 三个 git 钩子、注册
  union merge driver、写入一段受管理的 `.gitattributes` 区块，最后做自检。
  该命令是幂等的。
- **`zai czero verify`** 会在一次性仓库里制造真实的重叠写入和真实的合并，
  逐条检查它们是否真的被拦住。**它不会修改当前仓库。**
  判定分 `verified` / `partial` / `broken` 三档 —— 只要有一项没能跑起来，
  它就不会报告「已验证」。
- **`zai czero doctor`** 诊断目前哪几层还在生效，**`zai czero uninstall`**
  只移除 `init` 加进去的东西。

### 更新

`zai update` 会先显示它要执行的命令再升级（`--check` 只查看，`--yes` 跳过确认）。
编辑器开着或没开都能用。要卸载就用 `zai uninstall`。

## 核心功能

按「与其他工具的差别有多大」排序。第一条就是这个产品存在的理由。

### 1. 文件与行区间的归属，在写入时强制

智能体在编辑前先认领文件或行区间，锚点是**周围的内容**而不是行号。
如果某个重叠区域已经属于另一个在跑的智能体，git 钩子就会拒绝这次写入 ——
发生在写入时，而不是合并时。同一个文件的不同行是允许的，
这正是让智能体保持并行、而不是被整文件锁串行化的原因。
[行区间协调的原理 →](docs/conflict-zero.md)

### 2. 一块屏幕，看清每个智能体在做什么

把多个 AI CLI 并排放着，一眼就能看出哪个在思考、在编辑、在执行，或者在等你回话。
添加一个智能体只要两次点击，不用去回忆命令行。

### 3. 停滞与退出检测

Zaivern Code 看的是语义上的进展，而不是像素：不再推进的智能体会被报告为**停滞**，
意外退出则以通知的形式浮现。

### 4. 群发与定向指令

在同一个输入框里把一条指令发给所有在跑的智能体，也可以只发给其中一个。

### 5. 审批

默认就是「需要审批」。自动 YES 需要按会话显式开启，权限提升永远要人来点头，
MCP 环境变量的值一次也不会显示。

### 6. 手机遥控

在手机上查看进度、发送指令、批准操作、编辑文件。
同一个 Wi-Fi、[Tailscale](https://tailscale.com/) 或 SSH 隧道都可以。

### 7. 内置编辑器

不离开 Zaivern Code 就能审阅代码和智能体的改动，包括 Markdown、图片、PDF 和 CSV。
未保存的缓冲区在崩溃后会被恢复。

### 8. AI 团队运行 —— 交出一份 SPEC，得到一支受管理的开发团队

```sh
zai team run SPEC.md --agents 4
```

Zaivern 读取 SPEC，推导出 Goal 与 Definition of Done，构建任务图并展示计划。
按下 **Start Team** 后，它只启动计划真正需要的智能体，把任务分派下去，并推动
实现 → 验证 → 评审 → 修改 → 集成直到完成。

**不会因为智能体说“完成了”就算完成。** 任务只能沿着
`Running → Validating → Reviewing → Completed` 前进；若任务 ID 或智能体 ID
与分配不符、改动了负责范围之外的文件、没有运行或未通过验证命令、仍有未解决的
blocker，完成报告都会被拒绝。评审由**与写代码不同的会话**负责。
智能体报告的 `validation` 只作为**参考信息**保留：验证命令由 Zaivern 自己
执行，只有它自己实测通过后才会进入评审。
push、merge、deploy、权限提升和破坏性命令永不自动执行——它们会变成屏幕上
等待你决定的事项。

组织看板会显示团队负责人、各专业小组的泳道、每个父/子智能体、他们此刻正在做
什么、任务图的进度、测试与评审结果，以及**最需要你关注的那一件事**。

[AI 团队文档](docs/team.md)

另外还包含：插件，以及六种语言的界面。
[插件文档](docs/plugins.md) · [翻译文档](docs/translating.md)

## 工作原理

1. **启动** —— 在一个窗口里拉起智能体，或接管你已经在跑的那些。
2. **认领** —— 编辑前先占住文件或行区间，锚点是周围的内容。
3. **把关** —— git 钩子在重叠写入抵达合并之前就拒绝它。
4. **集成** —— 互不重叠的改动照常通过 git 合并。

## 支持的智能体

Claude Code · Codex · Gemini CLI · Cursor Agent · GitHub Copilot CLI ·
**另外 28 种** —— 启动预设共 33 种，另有 6 种可通过 ACP 驱动。

任意组合都可以，只用一个智能体也行。
没有你在用的？[提交集成请求](https://github.com/tacyan/zaivern-code/issues)。

## 为什么选 Zaivern

|  | 终端复用器 | 通用智能体面板 | Zaivern Code |
|---|:---:|:---:|:---:|
| 行区间归属 + 写入时拒绝 | ❌ | ❌ | ✅ |
| 知道智能体状态（思考 / 阻塞 / 停滞） | ❌ | 不一定 | ✅ |
| 一块屏幕看全部智能体 | ❌ | ✅ | ✅ |
| 审批以通知形式送达 | ❌ | 不一定 | ✅ |
| 手机 / 远程控制 | ❌ | 不一定 | ✅ |
| 单个原生二进制、无需运行时 | 不一定 | 不一定 | ✅ |

## 实测数据与限制

顶部那张 64 个智能体的表是合成仓库的数据。在**真实仓库**上，用
`tools/anyrepo-prove.sh` 克隆后以 16 个写入者重放（zai 0.14.0）：

| 仓库 | 原生 git | Zaivern Code |
|---|---|---|
| zaivern-code（Rust，259 个被跟踪文件） | 26 个文件冲突 / 28 个冲突块 | **0 / 0** —— 96/96 落地，0 次拒绝，30 次挪位 |
| hyperframes（TS/HTML，1,194 个被跟踪文件） | 26 / 28 | **0 / 0** —— 96/96 落地，0 次拒绝，32 次挪位 |

拒绝并不是唯一的结果。当认领撞车时，`--shift` 会把它挪到最近的、能放下同样宽度的空闲行区间 ——
这正是上面两行能全部落地、一次都没被拒的原因。

### “零冲突”意味着什么

- **归属永远成立。**「不会把同一行发给两个智能体」只取决于账本，与文件内容无关：
  独立重跑的 126 次里，126 次都是 `dup_lines = 0`。
- **能否干净合并则是有条件的。** 在重复性内容里（连续的代码围栏、生成代码、
  反复出现的同一行），即使认领的行区间隔得够远，git 仍可能产生冲突。
  关卡会**拒绝这样的认领**，而不是承诺一个它保证不了的合并。
- **语义冲突不在范围内。** 被阻止的是行归属的重叠；改了签名的一方和另一个文件里
  仍按旧写法调用的一方，不在其列。
- **互不相交的工作本来就不需要帮忙。** 隔得足够远的行区间，原生 git 本来就能零冲突合并。
  行区间归属找回来的是**按文件加锁所摧毁的并行度** —— 该比的是这个。
- **只在 git 能强制的地方才有强制力。** `zai lease claim` 在非 git 目录里也会成功，
  但那里什么都拦不住。`zai czero doctor` 会报告哪些仓库形态
  （worktree、submodule、sparse-checkout、LFS、bare）真正被覆盖。

以上任何一项都可复现：`tools/conflict-bench.sh`、`tools/coedit-bench.sh`、
`tools/anyrepo-prove.sh --repo .`
[完整方法与尚存的空缺 →](docs/conflict-zero.md) ·
[哪种仓库形态保证哪些性质 →](docs/czero-repo-shapes.md)

## 支持的平台

| 项目 | 支持情况 |
|---|---|
| 操作系统 | macOS arm64/x86_64、Linux x86_64/arm64、Windows x86_64 |
| 分发 | 单个原生二进制、无需运行时；每个版本附带校验和、SBOM 与构建来源证明 |
| AI CLI | 33 种启动预设，另有 6 种通过 ACP |
| 测试 | v0.23.0 共 5,005 个，在 CI 的 macOS、Linux、Windows 上运行 |
| 许可证 | Apache-2.0 |

## 文档

| 文档 | 涵盖内容 |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | 「零冲突」主张了什么、没有主张什么，以及背后的每一项测量 |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | 哪种仓库形态保证哪些性质 |
| [docs/idle-cost.md](docs/idle-cost.md) | 空闲 CPU 与二进制体积的测量方法 |
| [docs/plugins.md](docs/plugins.md) | 编写插件，以及[格式规范](docs/PLUGIN_SPEC.md) |
| [docs/team.md](docs/team.md) | `zai team`：SPEC 如何变成任务图、什么才算“完成”、哪些操作永不自动执行 |
| [docs/README.md](docs/README.md) | 其余全部文档的索引，按其支撑的主张分组 |

[发布说明](https://github.com/tacyan/zaivern-code/releases) ·
[安全策略](SECURITY.md) · [参与贡献](CONTRIBUTING.md)

## 试一试

在同一个仓库里用两个智能体试试：

```bash
zai czero init
zai .
```

启动两个智能体，让它们都指向同一个文件，看着第二个重叠的写入**在变成合并冲突之前**
就被拒绝。这就是这个产品的全部想法，大约一分钟就能看到。

如果它对你有用，点一个 ⭐ **Star** 能帮更多人找到它。

## 社区

- 发现了协调上的边界情况？[提个 issue](https://github.com/tacyan/zaivern-code/issues)。
- 在用还不支持的编程智能体？[提交集成请求](https://github.com/tacyan/zaivern-code/issues)。
- 在跑 8、16、32 或 64 个智能体？把你的数字分享出来 ——
  `tools/conflict-bench.sh` 和 `tools/anyrepo-prove.sh` 产出的结果可与上面的表格对比。

欢迎向 `main` 提交 Pull Request ——
[CONTRIBUTING.md](CONTRIBUTING.md) 讲了如何从源码构建（Rust 1.88+）、
如何验证改动，以及如何在本地跑 Linux 与 Windows 的检查。

## 许可证

[Apache License 2.0](LICENSE)
