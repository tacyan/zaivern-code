<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

### AI 에이전트 64개. 저장소 하나. 머지 충돌 제로.

**병렬 코딩 에이전트를 위한 조정 계층.**

Claude Code, Codex, Gemini CLI를 비롯한 코딩 에이전트를 같은 저장소에서 —— 머지 충돌에
휘둘리지 않고 —— 함께 돌립니다.

[English](README.md) | [日本語](README.ja.md) | [简体中文](README.zh-CN.md) | **한국어** | [Português (Brasil)](README.pt-BR.md) | [Español](README.es.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

<!-- TODO: 15~20초 벤치마크 데모로 교체할 것:
     한 저장소에 64개 에이전트 / 순수 git 132개 충돌 헝크 / Zaivern 0개.
     아래 GIF는 콕핏 화면이며, 조정 결과는 담고 있지 않다. -->
<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Claude Code, Codex, Gemini CLI 등을 나란히 실행하는 Zaivern Code" />
</a>

| 작성자 64명 · 같은 저장소 · 같은 작업량 | 순수 git | Zaivern Code |
|---|---:|---:|
| 충돌한 머지 | 64건 중 57건 | **0** |
| 충돌 헝크 | 132 | **0** |

[측정 방법과 대가, 한계 보기 →](docs/conflict-zero.md)

[**빠른 시작**](#빠른-시작) ·
[**측정 결과**](#측정-결과) ·
[**문서**](#문서) ·
[**다운로드**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**웹사이트**](https://zaivern.com/)

</div>

## 문제

코딩 에이전트 하나를 돌리는 건 쉽습니다. 넷은 그렇지 않습니다.
같은 파일을 건드리는 두 개만 있어도 충분히 겪습니다.

- 같은 줄을 고치고, 그 사실을 머지할 때 알게 됩니다.
- 어느 에이전트가 일하는 중인지, 막혔는지, 조용히 멈췄는지 보이지 않습니다.
- 보고 있지 않던 탭에서 승인 프롬프트가 지나가 버립니다.
- 통합은 매번 사람의 일이 됩니다.

병목은 에이전트가 아니라 **에이전트들 사이의 조정**입니다.

## 해결 방식

Zaivern Code는 각 코딩 에이전트가 저장소의 어느 부분을 안전하게 편집할 수 있는지를
조정합니다. 충돌을 머지할 때 발견하는 대신 **충돌하는 쓰기가 착지하기 전에** 잡아내고,
실행 중인 에이전트를 보고, 조종하고, 되살리는 곳을 한 군데로 모읍니다.

```text
Zaivern 없이                              Zaivern과 함께

에이전트 1  ─┐                            에이전트 1  ─┐
에이전트 2  ─┤                            에이전트 2  ─┤   ┌─────────────┐
에이전트 3  ─┼─→ 같은 파일 ─→ 머지 충돌    에이전트 3  ─┼─→ │ 라인 범위    │ ─→ 깔끔한
    ...     ─┤                                ...     ─┤   │    원장      │    통합
에이전트 64 ─┘                            에이전트 64 ─┘   └─────────────┘

충돌 헝크 132개                            충돌 헝크 0개
```

**64개가 필요한 이야기가 아닙니다.** 같은 파일을 건드리는 둘이면 충분합니다.
2개로 시작해 64개까지.

## 빠른 시작

먼저 지원되는 AI 코딩 CLI를 최소 하나 설치하고 로그인하세요 —— Zaivern Code에는
**33개**의 실행 프리셋이 들어 있지만, 시작하는 데는 하나면 충분합니다.

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

그다음 창에서 `+ Agent`를 눌러 이미 설치한 CLI를 고르고 작업을 보내면 됩니다.

저장소에 충돌 조정을 켜려면:

```bash
zai czero init      # 원장, git 훅, 머지 드라이버를 설치하고 자가 진단
zai czero verify    # 일회용 저장소에 실제 충돌을 만들어 막히는지 확인
```

설치 스크립트는 공개된 `checksums.txt`와 **압축을 풀기 전에** 대조하고, 맞지 않으면
중단합니다. [수동 다운로드, 체크섬 검증, 빌드 출처, SBOM →](SECURITY.md)

### 업데이트

```bash
zai update            # 새 릴리스를 확인하고, 명령을 보여준 뒤 업그레이드
zai update --check    # 확인만 하고 아무것도 바꾸지 않음
zai update --yes      # 확인 절차 없이 업그레이드
```

에디터 실행 여부와 무관하게 동작합니다. 제거는 `zai uninstall`.

## 핵심 기능

### 1. 머지 충돌에 휘둘리지 않고 병렬로 돌리기

에이전트는 편집 전에 파일이나 라인 범위를 선점합니다. 살아 있는 다른 에이전트가 이미 그
영역을 갖고 있으면, git 훅이 충돌하는 쓰기를 거절합니다 —— 머지할 때가 아니라 **쓰는
순간에**.

라인 범위가 겹치지 않게 배분한 64개 에이전트 벤치마크에서는 **64개** 전부가 반영되고
충돌 헝크는 **0**이었습니다. 파일 단위 리스였다면 정확히 1개만 통과했을 상황입니다.
[라인 범위 조정의 원리 →](docs/conflict-zero.md)

### 2. 병렬 에이전트 관리

여러 AI CLI를 나란히 놓고 무엇이 생각 중이고, 편집 중이고, 실행 중이고, 당신을 기다리는지
한눈에 봅니다. 에이전트 추가는 명령줄을 떠올리는 일이 아니라 두 번의 클릭입니다.

### 3. 상태와 정체(stall) 감지

Zaivern이 보는 것은 화면 픽셀이 아니라 **의미 있는 진행**입니다. 진행이 멈춘 에이전트는
**정체**로 보고되고, 예기치 않은 종료는 알림으로 올라옵니다.

### 4. 일괄 지시

입력창 하나에서 실행 중인 모든 에이전트에게 같은 지시를 보내거나, 하나만 지정할 수 있습니다.

### 5. 승인

기본값은 승인 필수입니다. 자동 YES는 세션 단위의 명시적 옵트인이고, 권한 상승은 항상
사람이 확인하며, MCP 환경 변수는 값을 표시하지 않습니다.

### 6. 휴대폰 원격

진행 확인, 지시 전달, 승인, 파일 편집을 휴대폰에서 합니다. 같은 Wi-Fi,
[Tailscale](https://tailscale.com/), 또는 SSH 터널을 쓸 수 있습니다.

### 7. 내장 에디터

Zaivern을 벗어나지 않고 코드와 에이전트의 변경을 검토합니다(Markdown, 이미지, PDF, CSV 포함).
저장하지 않은 버퍼는 크래시 후 복구됩니다.

이 밖에 플러그인 구조와 여섯 언어의 UI가 들어 있습니다.
[플러그인 문서](docs/plugins.md) · [번역 문서](docs/translating.md)

## 동작 방식

1. **실행** —— 창 하나에서 코딩 에이전트를 띄우거나, 이미 돌고 있는 것에 붙입니다.
2. **선점** —— 편집 전에 파일 또는 라인 범위를 주변 내용에 앵커를 걸어 선점합니다.
3. **게이트** —— 겹치는 쓰기를 머지에 닿기 전에 git 훅이 거절합니다.
4. **통합** —— 겹치지 않는 변경은 평소처럼 git이 머지합니다.

[기술적 세부 사항 →](docs/conflict-zero.md) ·
[어떤 보장이 어떤 저장소 형태에서 성립하는지 →](docs/czero-repo-shapes.md)

## 지원 에이전트

Claude Code · Codex · Gemini CLI · Cursor Agent · GitHub Copilot CLI ·
**그 외 28개** —— 실행 프리셋은 모두 33개이며, ACP로 구동 가능한 것이 6개 더 있습니다.

Zaivern Code는 AI 모델이 아니며 모델을 포함하지도 않습니다. 이미 설치하고 로그인해 둔
CLI를 구동할 뿐입니다. 어떤 조합이든, 하나만 써도 됩니다. 쓰는 게 없나요?
[연동을 요청해 주세요](https://github.com/tacyan/zaivern-code/issues).

## 왜 Zaivern인가

|  | 터미널 멀티플렉서 | 범용 에이전트 대시보드 | Zaivern Code |
|---|:---:|:---:|:---:|
| 여러 에이전트 동시 실행 | ✅ | ✅ | ✅ |
| 한 화면에서 전부 보기 | ❌ | ✅ | ✅ |
| 상태 인식(사고 중 / 대기 / 정체) | ❌ | 제품마다 다름 | ✅ |
| 라인 범위 소유권 + 쓰기 시점 거절 | ❌ | ❌ | ✅ |
| 승인을 알림으로 | ❌ | 제품마다 다름 | ✅ |
| 휴대폰 / 원격 제어 | ❌ | 제품마다 다름 | ✅ |
| 단일 네이티브 바이너리, 런타임 불필요 | 제품마다 다름 | 제품마다 다름 | ✅ |

## 측정 결과

**64개 에이전트, 한 저장소, 같은 작업량**(파일 수 = 작성자 × 6, 파일 겹침 50%):

| | 순수 git | Zaivern Code |
|---|---:|---:|
| 충돌한 머지 | 64건 중 57건 | **0** |
| 충돌 헝크 | 132 | **0** |

이 0은 쓰기를 거절해서 산 숫자입니다. 계획된 384건 중 202건이 반영되었고 나머지는
게이트에서 멈췄습니다. 라인 범위가 실제로 겹치지 않는 경우에는 64개 전부가 반영되고
거절은 0건입니다.

**이 저장소 자체에 16개 에이전트를 동시에**(zai 0.14.0): 순수 git은
**충돌 파일 26개 / 헝크 28개**. 원장을 끼우면 **0 / 0**이고 **96건의 편집이 모두 반영**
되었습니다(거절 0건, 그중 30건은 비어 있는 라인 범위로 옮겨 선점).

### "충돌 제로"가 뜻하는 것

- Zaivern은 겹치는 쓰기를 머지 충돌로 만들기보다 **거절**할 수 있습니다.
  충돌 수는 0이지만 처리량은 0이 아닙니다.
- 막는 것은 라인 소유권의 겹침입니다. **의미적 충돌은 감지하지 않습니다** ——
  한쪽이 시그니처를 바꾸고 다른 쪽이 옛 호출을 유지해도 머지는 깔끔하게 통과합니다.
- 충분히 떨어진 라인 범위는 애초에 도움이 필요 없습니다. 순수 git이 이미 충돌 없이
  머지하며, 라인 범위 소유권은 파일 단위 리스가 망가뜨린 병렬성을 되돌려 줄 뿐입니다.

[전체 방법론, 규모별 수치, 게이트 지연, 한계 →](docs/conflict-zero.md)

## 지원 플랫폼

| 항목 | 지원 |
|---|---|
| OS | macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64 |
| AI CLI | 실행 프리셋 33개, ACP 경유 6개 |
| 테스트 | 4,985개. CI에서 macOS, Linux, Windows |
| 라이선스 | Apache-2.0 |

## 문서

| 문서 | 내용 |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | "충돌 없음"이 주장하는 것과 아닌 것, 그리고 그 뒤의 모든 측정 |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | 어떤 보장이 어떤 저장소 형태에서 성립하는지 |
| [docs/plugins.md](docs/plugins.md) | 플러그인 작성과 [형식 사양](docs/PLUGIN_SPEC.md) |
| [docs/README.md](docs/README.md) | 나머지 모든 문서의 색인(뒷받침하는 주장별 분류) |

[유휴 CPU와 바이너리 크기 측정 →](docs/idle-cost.md) ·
[릴리스 노트](https://github.com/tacyan/zaivern-code/releases)

## 사용해 보기

병렬 코딩 에이전트가 일상의 일부라면, 다음 멀티 에이전트 작업에서 Zaivern Code를 돌려
보세요 —— 저장소에서 `zai czero init`을 실행하고, 같은 파일에 두 에이전트를 붙인 뒤,
두 번째 쓰기가 **엉망으로 머지되는 대신 거절되는 것**을 보는 것으로 충분합니다.

## 커뮤니티

- 조정의 빈틈을 찾았나요? [이슈를 열어 주세요](https://github.com/tacyan/zaivern-code/issues).
- 아직 지원되지 않는 코딩 에이전트를 쓰시나요? [연동을 요청해 주세요](https://github.com/tacyan/zaivern-code/issues).
- 8, 16, 32, 64개를 돌리고 있나요? 측정치를 공유해 주세요 —— `tools/conflict-bench.sh`와
  `tools/anyrepo-prove.sh`가 위 표와 비교 가능한 수치를 만들어 줍니다.
- Zaivern Code로 무언가 만들었나요? 구성을 보여 주세요.

풀 리퀘스트는 `main`으로 환영합니다 —— 소스에서 빌드(Rust 1.88+), 변경 검증,
Linux와 Windows 점검을 로컬에서 돌리는 방법은 [CONTRIBUTING.md](CONTRIBUTING.md)에 있습니다.

Zaivern Code가 도움이 되었다면 ⭐ **Star**가 다른 사람들이 이걸 찾는 데 도움이 됩니다.

## 라이선스

[Apache License 2.0](LICENSE)
