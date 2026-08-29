# AI Team Development Control Plane (`zai team`)

SPEC.md を渡すだけで、Zaivern が AI 開発チームを編成し、**タスク分解 →
担当割当 → 並列実装 → 検証 → レビュー → 修正 → 統合**まで面倒を見る層。

複数の AI ターミナルを並べる道具から、「共通 Goal・Definition of Done・
Task Graph・依存・担当範囲・親子関係・テスト結果・レビュー結果・再試行・
人へのエスカレーション・実行履歴・安全制限」を**一元管理する制御面**へ
進める、という位置づけ。

## 使い方

### CLI

```sh
zai team plan SPEC.md --agents 4          # 計画を見る (1 体も起動しない)
zai team plan SPEC.md --json              # 機械が読む形
zai team run  SPEC.md --agents 4          # GUI を起こして Plan Preview
zai team run  SPEC.md --agents 4 --yes    # Start Team の確認だけ省く
zai team status [--json]                  # 保存された状態
zai team resume                           # 未完了 Run を開き直す
zai team stop                             # 新規割当を止める (kill は承認)
zai team reset --dry-run                  # 消す対象を出すだけ
zai team reset --yes                      # 明示確認のうえで消す
```

`zai team run` は**ヘッドレスではない**。起動要求を投函してから Zaivern を
起こす。実行中のインスタンスがあれば二重起動せず、そちらが投函を拾って
Team 画面へ切り替える。`--headless` は MVP では未実装で、**黙って無視せず**
終了コード 2 と説明を返す。

### GUI

```
Zaivern → コマンドパレット → 🏛 Team
        → 🆕 New Team Run
        → SPEC ファイル or 直接入力 / エージェント数 / 最大試行 / 承認モード
        → Plan Preview
        → Start Team
```

初期値は `agents=4` / `max_attempts=3` / `review_required=true` /
`approval_mode=ask`。

**CLI 起動と GUI 起動で別々の実装を作らない。** どちらも
`TeamPanel::plan` → `TeamRuntime` の 1 本を通る
(`team::wiring_tests::cli起動とgui起動は同じruntimeを通る` が番人)。

## 層

```
SPEC / Goal
    ↓  planner.rs        (LLM に差し替え可能な境界。既定は決定的な StaticPlanner)
TeamPlan / Task Graph     plan_schema.rs / graph.rs / model.rs
    ↓  runtime.rs         (Reconciliation Loop。egui を知らない)
Deterministic Scheduler   scheduler.rs
    ↓
既存 Coordinator          crate::coordinator  ← 安全制御はここが持つ
    ↓
既存 Session / Terminal / Lease / Czero
    ↓
実際の Claude Code / Codex / Gemini CLI
```

GUI は Runtime の状態を**表示するだけ**:

```
TeamRuntime  ──TeamSnapshot──→  view_model.rs ──→ organization_board.rs / inspector.rs
             ←──TeamAction────  app/team_glue.rs
             ──TeamEffect────→  app/team_glue.rs (起動・送信・停止・検証)
```

## 再利用している既存機能 (作り直していないもの)

| 要件 | 使っている既存実装 |
| --- | --- |
| エージェント起動 / terminal tile | `agents::AgentManager` / `terminal` |
| `SessionState` の導出 | `app::coordinator_state` |
| 割当可否・前任者停止の確認・再試行上限 | `coordinator::try_assign` |
| ファイル重なりの fail-closed 判定 | `coordinator::admit` → `lease::overlaps` |
| パス正規化 | `lease::normalize_path` |
| 指示の送信 (Ink 系 TUI の取りこぼし対策込み) | `submit::Job` |
| 停止の承認ゲート | 既存の approval 経路 |
| コスト上限 | `queue_submit` の `cost_block_reason` |
| 端末の選択 | `app::focus_agent_in_place` |
| 置き場のワークスペースキー | `history::workspace_key` |
| 機能の登録 (共有ファイルを触らない) | `src/features/*.rs` + `build.rs` |

Team は**自分のメタデータだけ**を持ち、配る瞬間に既存 Coordinator へ橋渡し
する。`coordinator` が断ったら**配らない** — 迂回する経路は作っていない。

## 「完了」の関門

**「エージェントが完了と言った」だけでは `Completed` にしない。** 通る順序:

```
Running → Validating → Reviewing → Completed
```

`state_machine.rs` の表がこの順序を強制する (`Running → Completed` は
存在しない遷移)。さらに `result_parser.rs` が完了報告を次の理由で却下する:

* JSON が壊れている / 上限を超えている
* `task_id` / `agent_id` が担当と違う
* 担当外のファイルを変更している
* 検証コマンドを実行していない / 失敗している
* blocker が残っている

却下は黙って捨てず、理由をタスクの文脈へ足して本人へ返す。

### 自己申告は証跡にならない

報告の `validation` は**参考情報**として `reported_validation` へ入れるだけで、
`validation.runs` (正式な証跡) には 1 件も入れない。報告を受けたタスクは
`Validating` で止まり、**Zaivern 自身が `validation_commands` を実行した実測**
(`note_validation`) だけが決着をつける。

* 決着をつける場所は `settle_validation` **1 か所だけ**。レビュータスクを
  作るのもここだけ (`runtime.rs` の他の場所からは作らない)
* 実測が全部通ったときだけ `Reviewing` (レビュー不要なら `Completed`) へ進む
* 1 つでも落ちたら `fail_validation` — 試行回数を 1 つ使い、
  「報告では成功と書かれていたが実際には失敗した」旨を文脈へ足して
  `Failed` → `Ready` へ戻す。上限に達したら `NeedsUser`
* `validation_commands` が空なら「走らせるものが無い」ので即座に決着する
  (`ValidationState::settled`)。永久に `Validating` で止まらない
* `review_required = false` でも、**実測が終わるまで `Completed` にしない**

レビューは `REQUEST_CHANGES` なら指摘を文脈へ載せて `Ready` へ戻し、
最大試行回数 (既定 3) に達したら `NeedsUser` にして人へ上げる。

### 配り直しは、旧担当が止まってから

`Reassign` を押しても、その場で `Ready` へは戻さない。**「人が押した =
止まっている」は成り立たない**ため、旧担当のセッションが生きているかを
`live_session_of` で見て分岐する:

* 居ない → その場で回収 (`free_task`)。誰も書いていないので安全
* 居る → `reassign_pending` を立て、`DecisionKind::StopAgents` の
  `Decision` を作って `RequestHumanApproval` を出す。承認されるまで
  担当も状態もそのまま (拒否すればそのまま続行)
* 承認後、`Observation` の観測でセッションが消えたことを確かめてから
  `release_after_stop_confirmed` → `Ready` (`settle_reassign` が毎 tick 見る)

`Decision` は永続化されるので、**承認待ちのまま再起動しても消えない**。
Stop Team (全体停止) と同じ経路・同じ承認ゲートを共有していて、
`Decision.task_id` が `Some` なら 1 体、`None` なら全体を指す。

## Effect の一生

`TeamEffect` は Runtime が出す**要求**であって、出した時点では何も起きて
いない。実行するのは `app/team_glue.rs` で、**成否を必ず返す**。

```
Pending (まだ出していない)
  → Dispatched (出した。実行側の返事待ち)
     → Completed (成功の ACK が来た。二度と出さない)
     → (失敗の ACK / 記録を外す) → 次の tick でもう一度出る
```

* 記録は `EffectRecord { key, state, at }` で、冪等キーは種類ごとに違う —
  `start:{agent}` / `instr:{task}:{attempt}:{session}` / `stop:{session}` /
  `validate:{task}` / `decide:{冪等キー}`。同じキーの二重発行はしない
* **復元時に `Dispatched` を「済んだこと」にしない。** `Completed` だけを
  引き継ぎ、`Dispatched` のまま落ちたものは記録が消えるので**撃ち直される**
  (`done_effects` を丸ごと捨てるのではない — 成功したものは残す)
* 起動だけは例外的に、セッションに紐づいていない ManagedSession が居る
  ときに `start:{agent}` の記録を外す (孤児の回収)
* 刈り取り (`prune_effects`) の対象は `Completed` だけ

## 構造化プロトコル

エージェントには次の形で報告させる (`prompt.rs` が指示文へ埋め込む)。

```
[ZAI-TEAM-RESULT]
{"task_id":12,"agent_id":"agent-1","status":"completed",
 "summary":"JWT middlewareを実装","changed_files":["src/auth.rs"],
 "validation":[{"command":"cargo test auth","exit_code":0}],"blockers":[]}
[/ZAI-TEAM-RESULT]

[ZAI-TEAM-REVIEW]
{"task_id":12,"verdict":"APPROVE","findings":[]}
[/ZAI-TEAM-REVIEW]

[ZAI-TEAM-EVENT]
{"kind":"sub_agent_started","agent_id":"backend-test-1",
 "parent_id":"agent-1","role":"tester","task_id":12,"action":"tests を作成中"}
[/ZAI-TEAM-EVENT]
```

**囲われたブロックだけを読む。** 画面の地の文からサブエージェントを捏造しない
(`sub_agent_started backend-test-1` という平文は 1 バイトも解釈しない)。

親が報告しただけのサブエージェントは `ReportedSubAgent` として区別し、
**端末を開くボタンは無効にして理由を出す** — 押せるのに何も起きないボタンは
画面が嘘をついているのと同じ。

## 安全条件

`graph::check_command` が許可リスト方式で検証コマンドを絞る。
**MVP で自動実行しないもの**:

git push / PR 作成 / merge / rebase / reset / deploy / release / publish /
本番 DB 操作 / credential 操作 / 課金 / `rm -rf` 等の破壊的操作 /
`sudo` 等の権限昇格 / ワークスペース外への書き込み / 任意の shell 文字列。

* シェルのメタ文字 (`; | & > < \` $` 改行) を含む文字列は実行しない
* コマンド・引数・cwd を分けて扱い、`sh -c` を挟まない
* SPEC がこれらを指定していても計画へは入らない
* 必要になったら `NeedsUser` にして、理由・対象・影響・選択肢を出す

`Stop` は新規割当を即座に止めるが、**実行中エージェントの kill は
承認ゲートを通す** (`Decision` を立てて人の承認を待つ)。

## 永続化

```
~/.zaivern/team/<ワークスペースキー>/
  schema.json   run.json   goal.json   teams.json
  tasks.json    agents.json  decisions.json   events.jsonl
  launch.json   ← zai team run の投函箱 (拾うと同時に消える)
```

* 置き場は `history::workspace_key` から決める (自前で 16 桁キーを作らない)
* 一時ファイルへ書いて rename (原子的置き換え)
* `Instant` を永続化しない。時刻は全部 Unix 秒
* **壊れていても黙って初期化しない** — `<名前>.corrupt-<epoch>` へ退避して
  理由を返す
* 版が新しすぎるときは読まずに残す
* `events.jsonl` は追記専用で上限 5,000 行

再起動時に未完了 Run があれば `Resume / Open Read Only / Discard` を出す。
Discard は確認を挟む。**`Running` / `Assigned` だったタスクを無条件に
`Running` へ戻さない** — 担当を空けて `Ready` (試行上限に達していれば
`NeedsUser`) へ落とし、担当が確認できたときだけ進める。

## 性能の約束

* Runtime は**描画の外**で回す。GUI は不変スナップショットを読むだけ
* 画面走査は 400ms 間隔で、**前回以降に増えた行だけ**を読む
* Activity Feed は 60 行、子エージェント行は 6 行、イベントは 500 件が上限
* 点滅は `Stalled` と承認待ちだけ
* **Goal が終わったら再描画を 1 回も頼まない** (アイドルの費用はゼロへ戻る)
* 再描画の要求は `perf::repaint_after(ctx, …, "team")` で出所を残す

## 今回やっていないこと (別 PR)

本番 deploy / GitHub への自動 push / PR 自動作成 / 自動 merge /
複数マシン分散 / Cloud Agent Pool / Kubernetes / 長期自律経営 /
動的な組織再編学習 / エージェントの自己改変 / semantic conflict の完全検出 /
ドラッグ＆ドロップによる組織編集。

責務は分離してあるので、`TeamPlanner` を LLM 実装に差し替える・
`TeamEffect` に新しい種類を足す、という形で後から載せられる。
