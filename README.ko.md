<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

### 여러 코딩 에이전트를 머지 충돌 없이 돌리세요.

**2개로 시작해서 64개까지.**
Zaivern Code는 겹치는 수정이 파일에 닿기 전에 막습니다. 그래서 머지 충돌이 되지 않습니다.

Claude Code, Codex, Gemini CLI를 비롯해 이미 설치해 둔 30가지 에이전트 CLI를 한 창에서.
단일 네이티브 바이너리 — macOS, Linux, Windows.

[English](README.md) | [日本語](README.ja.md) | [简体中文](README.zh-CN.md) | **한국어** | [Português (Brasil)](README.pt-BR.md) | [Español](README.es.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

</div>

**설치하고 실행하기**

macOS / Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.sh | sh
zai .
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.ps1 | iex
zai .
```

지원되는 AI 코딩 CLI를 최소 하나는 설치하고 로그인해 두어야 합니다.
Zaivern Code는 이미 가지고 있는 CLI를 구동할 뿐, AI 모델이나 구독을 포함하지 않습니다.

**충돌 조정(선택):**

```bash
zai czero init
```

이 명령은 현재 Git 저장소를 변경합니다.
[변경 내용 미리 보기와 검증 →](#충돌-조정-활성화) ·
[수동 다운로드와 검증](SECURITY.md)

<div align="center">

<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Zaivern Code 콕핏: 여러 코딩 에이전트 CLI가 한 창에 나란히 떠 있고 각 에이전트의 상태가 표시된 화면" />
</a>

[**빠른 시작**](#빠른-시작) ·
[**측정 결과**](#측정-결과와-한계) ·
[**문서**](#문서) ·
[**다운로드**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**웹사이트**](https://zaivern.com/)

</div>

*위 영상은 콕핏입니다. 여러 에이전트 CLI가 한 창에 있는 모습이며, 충돌 조정의 결과는
담겨 있지 않습니다. 그쪽은 따로 측정했고, 바로 아래에 있습니다.*

## 실증

**에이전트 64개, 저장소 하나, 같은 작업량.** 파일 수 = 작성자 × 6이고 그중 절반은
둘 이상이 노립니다. 같은 작업 목록을 두 번 돌렸습니다. 한 번은 순수 git으로,
한 번은 Zaivern Code의 라인 범위 원장을 거쳐서.

| | 순수 git | Zaivern Code |
|---|---:|---:|
| 충돌한 머지 | 64건 중 57건 | **64건 중 0건** |
| 사람이 풀어야 한 충돌 헝크 | 132 | **0** |
| 실제로 반영된 수정 | 384건 중 384건 | 384건 중 202건 |
| 반영 전에 막은 쓰기 | 0 | 182 |

**이 0은 쓰기를 거절해서 산 것이지, 양쪽을 마법처럼 합친 결과가 아닙니다.**
계획된 384건 중 182건은 그 라인을 이미 살아 있는 다른 에이전트가 갖고 있었기 때문에
관문에서 멈췄습니다. 그중 14건은 혼잡으로 인한 일시 거절이며, 재시도하면 통과할 수 있습니다.

**라인 범위가 정말로 겹치지 않으면 한 건도 거절하지 않습니다.** 에이전트 64개가
*하나의* 파일의 서로 다른 64개 라인 범위를 편집하면 **64건 모두 반영**되고,
거절 **0**, 충돌 헝크 **0**입니다. 파일 단위 잠금이라면 1건만 통과하고 63건을 거절합니다.

의미적 충돌은 **감지하지 않습니다**. 한쪽이 시그니처를 바꾸고 다른 쪽이 옛 방식으로
계속 호출해도 둘 다 통과하며, git은 깔끔하게 머지합니다.

[측정 방법, 규모별 수치, 관문 지연, 남아 있는 모든 구멍 →](docs/conflict-zero.md)

## 문제

코딩 에이전트 하나를 돌리는 건 쉽습니다. 넷은 그렇지 않습니다.
**같은 파일을 건드리는 둘이면 이미 충분합니다:**

- 같은 줄을 고치고, 그 사실은 머지할 때가 되어서야 알게 됩니다.
- 어느 에이전트가 일하고 있고, 막혔고, 조용히 멈췄는지 보이지 않습니다.
- 보고 있지 않던 탭에서 승인 프롬프트가 스쳐 지나갑니다.
- 통합이 매번 당신의 일이 됩니다.

병목은 에이전트가 아닙니다. **에이전트들 사이의 조정**입니다.

## 해결 방식

Zaivern Code는 각 에이전트가 저장소의 어느 부분을 안전하게 편집해도 되는지를 조정합니다.
충돌을 머지 시점에 발견하는 대신, **충돌하는 쓰기가 반영되기 전에** 겹친 작업을 잡아냅니다.
그리고 돌아가는 에이전트를 보고, 조종하고, 되살리는 자리를 한 곳으로 모읍니다.

```text
Zaivern 없이                             Zaivern과 함께

Agent 1  ─┐                              Agent 1  ─┐
Agent 2  ─┤                              Agent 2  ─┤   ┌─────────────┐
Agent 3  ─┼─→ 같은 파일 ─→ 머지 충돌      Agent 3  ─┼─→ │ 라인 범위의 │ ─→ 깔끔한
   ...   ─┤                                 ...   ─┤   │    원장     │    통합
Agent 64 ─┘                              Agent 64 ─┘   └─────────────┘
```

## 빠른 시작

### 멀티 에이전트 콕핏 실행하기

이 페이지 위쪽의 한 줄 명령으로 설치한 뒤, 프로젝트 폴더에서 `zai .`을 실행하세요.
해당 폴더로 콕핏이 열립니다 — 에이전트 타일, 에디터, 휴대폰 원격.
`+ Agent`를 누르고 설치해 둔 CLI를 골라 작업을 넘기면 됩니다.
**이것만으로는 충돌 조정이 켜지지 않습니다.** 그건 다음 단계입니다.

인스톨러는 내려받은 아카이브를 릴리스의 `checksums.txt`와 **압축을 풀기 전에** 대조하고,
일치하지 않으면 중단합니다.
[수동 다운로드, 체크섬 검증, 빌드 출처, SBOM →](SECURITY.md)

### 충돌 조정 활성화

```bash
zai czero init --dry-run  # 예정된 변경 미리 보기
zai czero init            # 원장과 Git 연동 설치
zai czero verify          # 일회용 저장소에서 검증
zai .                     # 콕핏 실행
```

- **`zai czero init --dry-run`** 은 예정된 변경을 보여줄 뿐, 현재 저장소를 변경하지 않습니다.
- **`zai czero init` 은 현재 Git 저장소를 변경합니다.** 라인 범위 원장을 준비하고,
  `pre-commit` / `pre-applypatch` / `pre-merge-commit` git 훅을 추가하고,
  union merge driver를 등록하고, 관리 블록이 든 `.gitattributes`를 쓴 뒤 자가 진단합니다.
  멱등합니다.
- **`zai czero verify`** 는 일회용 저장소에서 실제로 겹치는 쓰기와 실제 머지를 일으켜
  하나하나가 정말 막히는지 확인합니다. **현재 저장소는 변경하지 않습니다.**
  판정은 `verified` / `partial` / `broken` 세 단계이며, 실행하지 못한 시도가 있으면
  "검증됨"이라고 보고하지 않습니다.
- **`zai czero doctor`** 는 지금 어느 계층이 살아 있는지 진단하고,
  **`zai czero uninstall`** 은 `init`이 넣은 것만 정확히 제거합니다.

### 업데이트

`zai update`는 실행할 명령을 보여준 뒤 업그레이드합니다(`--check`는 확인만,
`--yes`는 확인 없이). 에디터가 켜져 있든 아니든 동작합니다. 제거는 `zai uninstall`.

## 핵심 기능

Zaivern Code를 다른 도구와 얼마나 갈라놓는지 순으로 나열했습니다. 첫 번째가 이 제품의 존재 이유입니다.

### 1. 파일과 라인 범위의 소유권을, 쓰기 시점에 강제

에이전트는 편집 전에 파일이나 라인 범위를 확보합니다. 기준은 줄 번호가 아니라
**주변 내용**입니다. 겹치는 영역을 이미 다른 살아 있는 에이전트가 갖고 있으면
git 훅이 그 쓰기를 거절합니다 — 머지 때가 아니라 쓰기 때에.
같은 파일이라도 줄이 다르면 허용되며, 그래서 파일 전체 잠금처럼 직렬화되지 않습니다.
[라인 범위 조정의 원리 →](docs/conflict-zero.md)

### 2. 한 화면에서, 각 에이전트가 무엇을 하는지 보입니다

여러 AI CLI를 나란히 띄우고 어느 것이 생각 중인지, 편집 중인지, 실행 중인지,
당신의 답을 기다리는지 한눈에 봅니다. 에이전트 추가는 두 번의 클릭이지,
외워 둔 명령줄이 아닙니다.

### 3. 정체와 종료 감지

Zaivern Code가 보는 것은 픽셀이 아니라 의미적 진행입니다. 더 나아가지 못하는 에이전트는
**정체(stalled)** 로 보고되고, 예기치 않은 종료는 알림으로 떠오릅니다.

### 4. 일괄 지시와 개별 지시

하나의 입력창에서 돌아가는 모든 에이전트에게 같은 지시를 보내거나, 한 에이전트만 겨냥할 수 있습니다.

### 5. 승인

기본값은 승인 필수입니다. 자동 YES는 세션마다 명시적으로 켜야 하고, 권한 상승은 항상
사람이 판단하며, MCP 환경 변수 값은 한 번도 표시하지 않습니다.

### 6. 휴대폰 원격

진행 상황 확인, 지시 전송, 작업 승인, 파일 편집을 휴대폰에서. 같은 Wi-Fi,
[Tailscale](https://tailscale.com/), 또는 SSH 터널 어느 쪽이든 됩니다.

### 7. 내장 에디터

Zaivern Code를 벗어나지 않고 코드와 에이전트의 변경을 검토합니다. Markdown, 이미지,
PDF, CSV까지. 저장하지 않은 버퍼는 크래시 후에 복구됩니다.

### 8. AI 팀 실행 — SPEC 만 건네면 관리되는 개발 팀이 움직입니다

```sh
zai team run SPEC.md --agents 4
```

Zaivern 이 SPEC 을 읽어 Goal 과 Definition of Done 을 세우고, 태스크 그래프를
만들어 계획을 보여 줍니다. **Start Team** 을 누르면 계획에 필요한 만큼만
에이전트를 띄우고 담당을 나눈 뒤, 구현 → 검증 → 리뷰 → 수정 → 통합까지
끌고 갑니다.

**에이전트가 "끝났다"고 말했다고 완료가 되지는 않습니다.** 태스크는
`Running → Validating → Reviewing → Completed` 순서로만 진행되며, 태스크 ID 나
에이전트 ID 가 담당과 다르거나, 담당 범위 밖 파일을 건드렸거나, 검증 명령을
실행하지 않았거나 실패했거나, 남은 blocker 가 있으면 완료 보고는 거부됩니다.
리뷰는 **코드를 쓴 세션과 다른 세션**이 맡습니다.
에이전트가 보고한 `validation` 은 **참고 정보로만** 남기고, 검증 명령은
Zaivern 이 직접 실행합니다. 리뷰로 넘어가는 것은 **직접 측정한 결과가
통과했을 때뿐**입니다.

검증 명령은 허용 목록으로 그냥 통과시키지 않고 **위험도로 나눕니다**. 경로가
붙은 실행 파일 (`/tmp/cargo test`, `./cargo test`, `tools/python x.py`) 은
실행하지 않습니다 — basename 만 보면 실제로 `/tmp/cargo` 가 실행되기
때문입니다. push, merge, deploy, publish, 권한 상승, 파괴적 조작은 거부합니다.
그리고 **저장소 안의 코드를 실행할 수 있는 것** (`cargo test`, `npm test`,
`pytest`, `make`, `node`, `go test`) 은 **사람이 승인하기 전까지 한 줄도
실행되지 않습니다** — 테스트 본문, `build.rs`, `Makefile` 은 셸이 할 수 있는
일을 모두 할 수 있습니다. 실행에는 시간 제한이 있고, 팀을 멈추면 프로세스
트리째 종료되며, 성공·실패·시간 초과·중지·실행 불가·실행기 연결 끊김 중
하나로 반드시 끝납니다. 승인한 것의 내부까지 격리하지는 않습니다 — Zaivern 이
보장하는 것은 **무엇을 실행했는가**이지, 그 프로세스가 그 뒤에 무엇을 하는지가
아닙니다. push · merge · deploy ·
권한 상승 · 파괴적 명령은 절대 자동으로 실행하지 않고, 화면 위에서 당신이
판단할 항목이 됩니다.

Organization Board 에는 팀 리드, 전문 팀 레인, 모든 부모/자식 에이전트, 각자가
지금 무엇을 하고 있는지, 태스크 그래프의 진척, 테스트와 리뷰 결과, 그리고
**지금 가장 당신의 판단을 기다리는 것**이 나옵니다.

[AI 팀 문서](docs/team.md)

이 밖에 플러그인과 6개 언어 UI도 들어 있습니다.
[플러그인 문서](docs/plugins.md) · [번역 문서](docs/translating.md)

## 동작 방식

1. **실행** — 한 창에서 에이전트를 띄우거나, 이미 돌리던 것을 붙입니다.
2. **확보** — 편집 전에 파일이나 라인 범위를, 주변 내용을 기준으로 잡아 둡니다.
3. **관문** — git 훅이 겹치는 쓰기를 머지에 닿기 전에 거절합니다.
4. **통합** — 겹치지 않는 변경은 평소처럼 git이 머지합니다.

## 지원 에이전트

Claude Code · Codex · Gemini CLI · Cursor Agent · GitHub Copilot CLI ·
**그 외 28가지** — 실행 프리셋은 모두 33가지이고, ACP로 구동 가능한 것이 6가지 더 있습니다.

어떤 조합이든 동작하며, 하나만 써도 됩니다.
쓰는 게 없나요? [연동을 요청해 주세요](https://github.com/tacyan/zaivern-code/issues).

## 왜 Zaivern인가

|  | 터미널 멀티플렉서 | 일반 에이전트 대시보드 | Zaivern Code |
|---|:---:|:---:|:---:|
| 라인 범위 소유권 + 쓰기 시점 거절 | ❌ | ❌ | ✅ |
| 에이전트 상태를 앎(생각 중 / 막힘 / 정체) | ❌ | 제각각 | ✅ |
| 모든 에이전트를 한 화면에 | ❌ | ✅ | ✅ |
| 승인이 알림으로 도착 | ❌ | 제각각 | ✅ |
| 휴대폰 / 원격 제어 | ❌ | 제각각 | ✅ |
| 단일 네이티브 바이너리, 런타임 불필요 | 제각각 | 제각각 | ✅ |

## 측정 결과와 한계

맨 위의 64개 표는 합성 저장소의 수치입니다. **실제 저장소**를 복제해
`tools/anyrepo-prove.sh`로 작성자 16개를 돌리면(zai 0.14.0):

| 저장소 | 순수 git | Zaivern Code |
|---|---|---|
| zaivern-code(Rust, 추적 파일 259개) | 26개 파일 충돌 / 28 헝크 | **0 / 0** — 96/96 반영, 거절 0, 이동 30건 |
| hyperframes(TS/HTML, 추적 파일 1,194개) | 26 / 28 | **0 / 0** — 96/96 반영, 거절 0, 이동 32건 |

거절만이 유일한 결말은 아닙니다. 확보가 부딪히면 `--shift`가 같은 폭이 들어가는
가장 가까운 빈 라인 범위로 옮깁니다. 위 두 줄이 한 건도 거절 없이 전부 반영된 이유가 이것입니다.

### "충돌 제로"가 뜻하는 것

- **소유권은 언제나 성립합니다.** "같은 줄을 두 에이전트에게 주지 않는다"는 원장만으로
  결정되며 파일 내용과 무관합니다: 독립적으로 다시 돌린 126회 모두 `dup_lines = 0`.
- **깔끔한 머지는 조건부입니다.** 반복적인 내용(연속된 코드 펜스, 생성된 코드,
  같은 줄의 반복)에서는 확보한 범위가 충분히 떨어져 있어도 git이 충돌을 낼 수 있습니다.
  관문은 보장할 수 없는 머지를 약속하는 대신 **그런 확보를 거절합니다**.
- **의미적 충돌은 범위 밖입니다.** 막는 것은 라인 소유권의 겹침이고,
  시그니처를 바꾼 쪽과 다른 파일에 남은 옛 호출부의 조합은 막지 못합니다.
- **떨어진 작업에는 애초에 도움이 필요 없습니다.** 충분히 떨어진 범위는 순수 git이
  원래부터 0건으로 머지합니다. 라인 범위 소유권이 되돌려 주는 것은
  **파일 단위 잠금이 망가뜨린 병렬성**이며, 비교 대상은 그쪽입니다.
- **git이 강제할 수 있는 곳에서만 강제됩니다.** `zai lease claim`은 git이 아닌 폴더에서도
  성공하지만 거기서는 아무것도 막히지 않습니다. 어떤 저장소 형태
  (worktree, submodule, sparse-checkout, LFS, bare)까지 실제로 보호되는지는
  `zai czero doctor`가 알려 줍니다.

무엇이든 재현할 수 있습니다: `tools/conflict-bench.sh`, `tools/coedit-bench.sh`,
`tools/anyrepo-prove.sh --repo .`
[전체 방법론과 남은 구멍 →](docs/conflict-zero.md) ·
[어떤 저장소 형태에서 무엇이 보장되는가 →](docs/czero-repo-shapes.md)

## 지원 플랫폼

| 항목 | 지원 |
|---|---|
| OS | macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64 |
| 배포 | 단일 네이티브 바이너리, 런타임 불필요. 릴리스마다 체크섬·SBOM·빌드 출처 증명 |
| AI CLI | 실행 프리셋 33가지, ACP로 6가지 추가 |
| 테스트 | v0.23.0 기준 5,005개, CI의 macOS·Linux·Windows에서 실행 |
| 라이선스 | Apache-2.0 |

## 문서

| 문서 | 다루는 내용 |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | "충돌 없음"이 주장하는 것과 주장하지 않는 것, 그리고 그 뒤의 모든 측정 |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | 어떤 저장소 형태에서 무엇이 보장되는가 |
| [docs/idle-cost.md](docs/idle-cost.md) | 유휴 CPU와 바이너리 크기를 측정하는 방법 |
| [docs/plugins.md](docs/plugins.md) | 플러그인 작성법과 [형식 명세](docs/PLUGIN_SPEC.md) |
| [docs/team.md](docs/team.md) | `zai team`: SPEC 이 어떻게 태스크 그래프가 되는지, 무엇이 "완료"의 관문인지, 무엇을 자동 실행하지 않는지 |
| [docs/README.md](docs/README.md) | 나머지 모든 문서의 색인, 뒷받침하는 주장별로 정리 |

[릴리스 노트](https://github.com/tacyan/zaivern-code/releases) ·
[보안 정책](SECURITY.md) · [기여 안내](CONTRIBUTING.md)

## 사용해 보기

같은 저장소에서 에이전트 두 개로 시험해 보세요:

```bash
zai czero init
zai .
```

에이전트 두 개를 띄워 같은 파일을 가리키게 하고, 두 번째의 겹치는 쓰기가
**머지 충돌이 되기 전에** 거절되는 것을 지켜보세요. 이게 이 제품의 전부이고,
1분이면 확인할 수 있습니다.

쓸 만하다면 ⭐ **Star** 하나가 다른 사람들이 찾는 데 도움이 됩니다.

## 커뮤니티

- 조정의 빈틈을 찾으셨나요? [이슈를 남겨 주세요](https://github.com/tacyan/zaivern-code/issues).
- 아직 지원되지 않는 코딩 에이전트를 쓰시나요? [연동을 요청해 주세요](https://github.com/tacyan/zaivern-code/issues).
- 8, 16, 32, 64개를 돌리고 계신가요? 수치를 공유해 주세요 —
  `tools/conflict-bench.sh`와 `tools/anyrepo-prove.sh`가 위 표와 비교 가능한 결과를 냅니다.

Pull Request는 `main`으로 환영합니다 —
[CONTRIBUTING.md](CONTRIBUTING.md)에 소스에서 빌드하는 법(Rust 1.88+),
변경을 검증하는 법, Linux와 Windows 검사를 로컬에서 돌리는 법이 있습니다.

## 라이선스

[Apache License 2.0](LICENSE)
