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

## 計画を作るのは誰か

**LLM ではない。** 現在の `TeamPlanner` の実装は決定的な `StaticPlanner`
1 つだけで、SPEC の見出し・箇条書き・「完了条件」「検証」節を**構文として**
読む。意味を解釈しないので、同じ SPEC からは必ず同じ計画が出る
(番人: `planner::tests::静的プランナーは決定的`)。

```text
いま:     SPEC → StaticPlanner (決定的) → validate_plan → TeamPlan → Task Graph
これから: SPEC → LLM TeamPlanner        → validate_plan → 同じ TeamPlan → Task Graph
```

`TeamPlanner` は trait なので、LLM 実装は**同じ `TeamPlan` を返す**形で
後ろに入る。差し替えても `validate_plan` の関門と Task Graph から先は
1 バイトも変わらない — 「AI が仕様を理解して分解する」ようになる日が来ても、
配る前の検証は同じ場所で効く。

## 層

```
SPEC / Goal
    ↓  planner.rs        (差し替え可能な境界。**いまある実装は決定的な
                          StaticPlanner 1 つだけ** — LLM Planner は未実装)
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
* 担当外のファイルを**実際に**変更している (自己申告ではなく実測)
* 変更されたファイルを実測できない (測れないなら完了にしない)
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
* 実行そのものは危険度で分けて、リポジトリのコードを走らせるものは
  **人の承認を通してから**動く (下の「安全条件」)
* `review_required = false` でも、**実測が終わるまで `Completed` にしない**

### 変更ファイルも実測する

報告の `changed_files` も**エージェントが中身を決められる**。書き忘れても
意図的に省いても、こちらから見れば同じ「空の配列」になるので、これを
担当範囲の照合に使うと担当外の変更が素通りする — しかも台帳には
「担当内しか触っていない」と残るので、後から気付けない。

```text
配る直前      → 基準点 (changeset::capture_baseline)
エージェント作業
完了報告      → 測る (changeset::measure) → 帰属 (attribute) → 照合
```

* 指紋は**内容のハッシュ**。`git status` の状態文字だけでは、基準点で
  もう `M` だったファイルの再変更が 1 バイトも見えない
* 判定は「`git status` に載っているか」× 内容。載っていない = HEAD と
  同じ、という git の意味をそのまま使う
* rename は `--no-renames` で「消えた + 増えた」の 2 件。**片方だけが
  担当範囲の中**、という形を見逃さない
* シンボリックリンクは辿らない (辿ると向き先の差し替えが見えない)。
  ワークスペース境界は**親で**判定する — 最後まで辿ると、中にある
  「外を指すリンク」そのものが外だと判定される一方、`link/x` は内側になる
* **並列作業の切り分けは既存のファイル所有リース** (`lease::overlaps`)。
  リースは範囲が互いに素であることを保証しているので、他人が握っている
  範囲の変更は自分のものではないと言い切れる。誰の範囲でもない変更は
  **自分ではないと言えない**ので安全側 (担当外) へ倒す
* **「他人のもの」と言えるのは、Coordinator が「いま押さえている」と
  言える範囲だけ** (`coordinator::occupies` — `admit` が割り当ての可否に
  使うのと同じ判定)。**計画に載っている他タスクの `files` を全部除いては
  いけない** — 除くと、まだ 1 度も配られていないタスクの範囲へ書き込んだ
  違反が消える (計画に `src/b.rs` を持つタスクが存在するだけで、誰でも
  そこを書き換えられる)。Team 側に 2 つ目の所有台帳は作らない
* 隣が完了して手放した範囲は、もう誰も押さえていない。そこへの変更は
  担当外として上げる — **見逃しより誤検知のほうが軽い**。誤検知は人が
  見れば分かるが、見逃した違反は台帳に「担当内だけ」と残って誰も
  気付けない
* **測れないなら完了にしない。** Git 管理外は `Unavailable` を返して
  人へ渡す (作業ツリー全体を歩く実装は `node_modules` も `target` も
  舐めるうえ .gitignore を自前で解釈することになるので採らない)
* 自己申告は `reported_files` に分けて残し、食い違いを文脈と事象へ出す。
  「報告に無いが実際に変更された」は把握していない印、「報告だけで実体が
  無い」はやったつもりの印
* レビュアーへ渡すファイル一覧も実測のほう。自己申告を渡すと、書き忘れた
  ファイルはレビューの対象にすらならない
* **担当範囲が宣言されていないタスクには「範囲外」も無い。** 測った事実は
  残すが、範囲の照合はしない (何を触ってよいかこちらが言っていない)

### 検証の出力は捨てない

`cargo test` が落ちたことだけ分かっても直せない。落ちたテスト名も
コンパイルエラーも対象行も、道具は stdout / stderr に書いている。

* 実行中に**並行して**読む。終わってからまとめて読む形にすると、パイプの
  バッファ (unix で 64KiB) が埋まった時点で子は書き込みで止まり、こちらは
  終了を待つので二度と進まない
* **末尾**を持つ (stdout 32KiB / stderr 64KiB)。失敗の理由は最後に出る。
  超えたら `truncated` を立てる — 黙って切ると「これで全部だ」と誤解される
* 成功したものは 2KiB だけ。ゼロにはしない (走ったことの跡が消える)
* 時間切れ・停止でも拾えた分は回収する
* ANSI エスケープと制御文字を落とし、**報告マーカーも無害化する** —
  検証の出力はエージェントが中身を決められるので、そのまま指示文へ入れると
  `[ZAI-TEAM-RESULT]` を仕込んで偽の完了報告を通せる
* 失敗したら診断を `context` へ積み、次に配る指示文へ載せる。Inspector
  にも出す (人が同じコマンドを手で打ち直さずに済むように)

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

## workspace の権限

**`zai team run` の投函 (`launch.json`) は未信頼データである。** ファイルは
`~/.zaivern/team/<キー>/` にあり、そのマシンで動く任意のプロセスが書ける。

```
workspace の権限を持つのは、いま開いている workspace だけ
```

* 判定の基準は**呼び出し側が渡した現在の workspace**。要求の中の
  `workspace_root` を基準にすると、それを `/` に書き換えるだけで
  `spec_path.starts_with(workspace_root)` が必ず通る (境界にならない)
* `launch::request_matches_workspace` が 2 つを見る:
  `canon(req.workspace_root) == canon(現在の workspace)` と
  `canon(req.spec_path).starts_with(canon(現在の workspace))`
* 正規化は実在すれば `canonicalize` (symlink を辿り `..` を畳む)、
  実在しなければ形だけ畳む (`lexical_normalize`)。**素のまま比べない** —
  `a/../b` と `b` が別物のままだと、形の違いだけで通ったり落ちたりする
* GUI 側は `attach_workspace(&req.workspace_root)` を**呼ばない**。
  置き場 (`state_dir`) も検証の cwd (`ValidationSpec.cwd`) も
  `panel.workspace` から決まるので、ここを要求に決めさせると
  「投函箱を書き換えるだけで別のフォルダを Team Run にする」ができる
* 番人は `launch::tests::要求の中のworkspaceは権限を持たない` ほか 5 本と、
  `wiring_tests::起動要求にworkspaceを決めさせない` (GUI 経路はヘッドレスの
  テストから回せないので、ソースの形で固定している)

### 実行コンテキストは運ぶ (取り直さない)

**Runtime が決めた実行先を、橋渡し層が「いまの画面の値」で取り直しては
いけない。** 取り直すと、Run を作ったあとに利用者がフォルダを選び直した
だけで、Team が面倒を見ているのとは違う場所でエージェントが動き出す。

```
TeamRuntime.workspace
    == AgentLaunchSpec.workspace_root   (発行時に焼き付ける)
    == launch_preset_with(.., cwd, ..)  (実行時に渡す)
    == ValidationSpec.cwd
```

* `app/team_glue.rs` の起動経路は `launch_preset(i, ctx)` を**使わない**
  (呼んだ瞬間の `agent_cwd()` を使うため)。`launch_preset_with` へ
  `spec.workspace_root` を渡す
* SPEC の解決も Team の workspace が基準 (`panel.workspace()`)。画面の
  いまのフォルダと食い違いうる — 実行中は切り替えを断るため
* 番人は `wiring_tests::実行コンテキストを画面のいまの値で取り直さない` と
  `runtime_tests::起動要求はruntimeのworkspaceを運ぶ`
* 指示も同じで、**宛先のタスクを運ぶ** (`SendInstruction.task`)。実行側が
  セッションから引き直すと、間に 1 tick 入っただけで別のタスクを指す

### Effect には持ち主がいる

発行した Effect には `RunOwner { run_id, workspace }` を焼き付け、
**実行の直前にもう一度突き合わせる**。一致しないものは実行しない。

キューを空にする「偶然」に頼らないのが要点で、workspace を切り替えても、
Run を作り直しても、前の Run の仕事が新しいところで動くことはない。
捨てたことは画面の帯に出る (黙って消えると理由を追えない)。

### workspace は切り替えられないことがある

面倒を見ているもの (セッションの付いたエージェント / 走っている検証 /
未実行の Effect) があるあいだは、**別の workspace へ切り替えない**。
切り替えると Runtime への参照が消えて、画面から消えたのに裏で動き続ける
プロセスが残る。

* 判断は `TeamPanel::live_work()`。**`runtime.is_some()` では広すぎる** —
  計画しただけ・停止し終えた Run で永久に切り替え不能になる
* 断った理由はそのまま画面に出す (黙って無視しない)
* 切り替えてよいときも、捨てる前に検証を止め・未実行の Effect を捨て・
  保存してから手放す
* アプリを閉じるときは `TeamPanel::shutdown()` — 検証を**その場で**
  プロセスツリーごと落とし (札を立てるだけでは worker ごと消えて誰も
  落とさない)、保存して Runtime を手放す。状態は `thread_local!` に居て
  アプリより長生きするので、持ったままにすると次のアプリが「もう居ない
  セッションへ結び付いた Runtime」を見る

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
  `start:{agent}` / `instr:{task}:{agent}:{attempt}:{配った回数}` / `stop:{session}` /
  `validate:{実行 ID}` / `decide:{冪等キー}`。同じキーの二重発行はしない
* **指示の鍵には「配った回数」(`dispatch_seq`) を混ぜる。** 試行回数は
  失敗のときしか増えないので、同じ担当へ同じ試行回数で配り直すと鍵が
  一致し、**指示が 1 行も届かないまま `Running`** になる (`Blocked` からの
  Retry で実際に踏んだ)
* **復元時に `Dispatched` を「済んだこと」にしない。** `Completed` だけを
  引き継ぎ、`Dispatched` のまま落ちたものは記録が消えるので**撃ち直される**
  (`done_effects` を丸ごと捨てるのではない — 成功したものは残す)
* 起動だけは例外的に、セッションに紐づいていない ManagedSession が居る
  ときに `start:{agent}` の記録を外す (孤児の回収)
* 刈り取り (`prune_effects`) の対象は `Completed` だけ

## 役割とプリセット

Organization Board は Architect / Implementer / Reviewer / QA / Integrator
といった役割を出すが、**この版では全員が同じエージェントプリセットで動く**。

* 選択は純関数 1 本 (`roles::preset_for_role`)。名前に役割の綴りを含む
  AI CLI があればそれ、無ければ最初の AI CLI
* 既定の設定に役割名のプリセットは無いので、実際にはほぼ後者に落ちる
* 「選べるのに効かない設定」は画面に出していない
* Role → Capability → Provider → Execution Target への差し替え点は
  この関数 1 か所。SSH / Cloud / 複数 PC はここから伸ばす

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

### 検証コマンドは危険度で分ける

**「許可リストに載っているから安全」でも「名前が整形ツールだから安全」でも
ない。** `cargo test` にシェルのメタ文字は 1 つも無いが、`build.rs` /
テスト本体 / `conftest.py` / `Makefile` / `package.json` の `scripts` を
通じてリポジトリ内の任意コードを実行できる。`black --check .` は読むだけ
だが `black .` は**ファイルをその場で書き換える** — 同じ実行体、同じ
許可リスト、旗ひとつで意味が変わる。

`graph::classify` は**実行体と引数の両方**を見て 4 段に分ける:

| 危険度 | 意味 | 例 | 扱い |
| --- | --- | --- | --- |
| `ReadOnly` | リポジトリのコードを実行せず、workspace も書き換えない | `shellcheck x.sh` / `black --check .` / `ruff check .` / `rustfmt --check src/a.rs` | **自動実行してよい唯一の段** |
| `RepositoryCodeExecution` | リポジトリ内のコードを実行しうる | `cargo test` / `npm test` / `pytest` / `make` / `node` / `go test` | 人が承認するまで 1 行も実行しない |
| `WorkspaceMutation` | workspace を書き換えうる | `black .` / `ruff check --fix .` / `ruff format .` / `rustfmt src/a.rs` | **MVP では自動実行しない。** 明示の承認を通す |
| `Forbidden` | 実行しない | パス指定 / シェルのメタ文字 / `git push` / `cargo publish` / `sudo` / `rm -rf` | 承認しても実行しない |

* 読むだけだと言い切れる形は表で持つ (`read_only_mode`)。`ruff` は
  サブコマンドまで見る (`check` は読むだけ / `format` は `--check` が要る)。
  **どちらとも言い切れないものは書き換える側へ倒す**
* **危険な旗を数え上げる (deny) 形では守れない。** 実際に 4 通り抜けた:
  `rustfmt --check --print-config default out.toml` (整形モードより手前で
  処理されるので `--check` で止まらず、指定パスへ書く) /
  `black --extend-exclude --check .` (Click が次の語を値として食うので
  `--check` は旗ではない) / `ruff check --fix-only .` と `--add-noqa .`
  (`--fix` とは綴りが違う) / `shellcheck -x a.sh` (`# shellcheck source=…`
  を辿ってディスクのどこでも読む)。
  数える側を逆にして、道具ごとに**知っている旗の集合**を持つ —
  そこに無い旗が 1 つでもあれば「読むだけ」とは言わない。
  値を食う旗も表に持ち、`flags_in_flag_position` が食われた語を旗と
  読まない
* 迷ったら `ReadOnly` に入れない — 設定ファイルから任意のコードを読み込む
  もの (`eslint` / `prettier` の JS 設定、`mypy` / `pylint` のプラグイン) は
  「検査するだけ」に見えても実行しうる
* `ReadOnly` が守るのは「**人のファイルを書き換えない**」。道具が自分の
  キャッシュ (`ruff check` の `.ruff_cache/` など) を作るのは止めない —
  止めるには `--no-cache` を強制するしかなく、それは人が書いたコマンドを
  勝手に変えることになる

### 判定した実体と、OS が起こす実体を一致させる

```
Planner → ValidationCommand{executable, args}
        → 危険度の判定 (名前と引数を見る: graph::classify)
        → 承認ゲート (runtime::advance)
        → 実行器で危険度と承認を**もう一度**確かめる (launch)
        → 実体の解決 (PATH を自分で引く)
        → その実体 + argv + cwd で起動
```

* **コマンドは構造のまま運ぶ** (`ValidationCommand`)。文字列へ戻すのは
  画面と台帳の見出しだけで、そこから実行経路へは戻らない。
  `split_whitespace` は引用符を知らないので、
  `cargo test --package "my package"` が 2 つの引数に割れる
* **PATH は自分で引く** (`validation_command::resolve_in`)。
  `Command::new("rustfmt")` に任せると OS がもう一度引くので、
  `PATH=<workspace>/bin:$PATH` に偽物を置くだけで乗っ取られる。
  確定した絶対パスをそのまま `Command` へ渡す
* **「workspace の外なら信用できる」は成り立たない。** エージェントは
  Zaivern と同じ利用者権限で動くので、`~/.local/bin` / `~/bin` /
  `%LOCALAPPDATA%` へ実行体を置ける
  (`mkdir -p ~/.local/bin && cp evil ~/.local/bin/rustfmt`)。
  置き場所は 4 つに分ける (`ExecTrust`):

  | 区分 | 例 | 扱い |
  |---|---|---|
  | `Workspace` | workspace の内側 / 相対 / 空の PATH 要素 | **承認があっても起こさない** |
  | `UserWritable` | `$HOME` 配下 / `/tmp` / **`/usr/local` `/opt/homebrew` `/opt/local`** / `%LOCALAPPDATA%` / `%ProgramData%` | 承認の証跡が要る |
  | `Unknown` | どれとも言えない場所 | 承認の証跡が要る |
  | `SystemTrusted` | `/usr/bin` `/bin` `/sbin` `C:\Windows` `C:\Program Files` | 危険度どおり |

  **Homebrew の場所をシステム扱いにしない。** Homebrew は
  `/opt/homebrew` (Apple Silicon) と `/usr/local` (Intel) を
  **ログインユーザーの所有**にするので、エージェントは Zaivern と同じ権限で
  `/opt/homebrew/bin/rustfmt` を書き換えられる — 「`~/.local/bin` は危ないが
  Homebrew は安全」という区別は成り立たない。MacPorts の `/opt/local` と
  Linuxbrew も同じ。`/usr/local` が `/usr` に含まれても通らないのは、
  利用者の場所をシステムより**先に**見るため。

  **綴りだけで決めない。** 表がシステムだと言っても、実体か置き場が
  *実際に*書き換えられるなら降格する (`measured_trust` — 世界書き込み可か、
  自分の uid が所有していて書き込み権がある場合)。**降格にだけ効く**ので、
  綴りで断ったものが実測で通ることはない。

  区分は `classify_path` (**純関数**) が決め、Windows の規則も
  `windows_policy` へ `env` を渡す形にして **macOS / Linux の CI から
  そのまま試験する** (cfg で分けると Windows のランナーでしか動かない
  判断が住み着く)
* **後ろへ落ちない。** PATH の順に見て最初に見つかった実行体が答え。
  前方の信用できないものを飛ばして後方の信用できるものを採ると、
  OS が実行するのは前方なのに判定は後方のものになる
* **見るのは置き場所だけではない。解決した実体そのものも見る。**
  `~/.local/bin` のような普通の PATH 要素から workspace の中へ
  シンボリックリンクを 1 本張れば、エージェントが書いたコードが
  「読むだけの検証」として動く (`std::fs::metadata` はリンクを辿るので、
  「そこにファイルがある」は判定にならない)
* **承認ゲートは実行の直前にもある。** `ValidationSpec.approved` が承認の
  証跡を実行器まで運び、`ReadOnly` 以外はそこに載っているものだけ走る。
  ゲートが 1 か所にしか無い状態は、そこを迂回されたときに何も残らない
* **危険度は名前についた評価。実体の区分と両方を見る。** 「読むだけ」と
  判定されても、その名前がどの実体を指すかは PATH が決める。無承認で
  起こしてよいのは **`SystemTrusted` の実体だけ**で、それ以外は
  `ReadOnly` でも承認の証跡が要る (承認済みなら起こす —
  「常に断る」にすると、承認そのものが意味を失う)
* **シェルは 1 段も挟まない。** `sh -c` も `cmd /C` も使わない。
  Windows の `.cmd` / `.bat` は `PATHEXT` で実体を解決し、そのパスを
  std へ渡す (バッチの引数の逃がし方は std が持っている)。
  **`PATHEXT` は素の名前より先に当てる** — 逆にすると、npm / yarn /
  pnpm のように「拡張子なしの sh スクリプト」と `.cmd` が同じ場所に
  並ぶ道具で、CreateProcess が起こせない側を選ぶ
* `SHELL_METACHARS` に入れるのは **std が安全に逃がせない字だけ**
  (`% !` と改行、それに unix の `; | & > < ` $`)。
  `^ ( )` は入れない — std のバッチ用の逃がし方が面倒を見るうえ、
  `pytest -k "not (slow or db)"` のような当たり前の SPEC が
  `Forbidden` → `NeedsUser` で行き止まる。**fail-closed でも
  「誰も直せない止まり方」は守りにならない**

### 落ちても消えず、二度も走らない (Effect の台帳)

Effect は**渡した瞬間には済んでいない**。台帳 (`RunDoc::effects`) は
「渡した」と「成立した」を別の段で持つ:

```text
作る → Dispatched (渡した。まだ成立していない) → Completed (本当に成立した)
                    ↓ 成立しなかった
                  記録ごと外す (= もう一度出せる)
```

立て直し (`TeamRuntime::restore`) は **`Completed` だけを引き継ぐ**。
`Dispatched` のまま落ちたものは記録が無いところから始まるので、もう一度出る。

| Effect | 「成立した」とは | 落ちたとき |
|---|---|---|
| `SendInstruction` | 相手の端末へ**確定まで届いた** (`submit::Act::Done`) | 届いていなければ担当を解いて配り直す |
| `StartAgent` | セッションへ結び付いた | 目印で引き取る (下記)。無ければ起こし直す |
| `RunValidation` | 裏で走らせ始めた | **決着していない `Completed` は引き継がない** (引き継ぐと永久に止まる) |
| `StopAgent` | 相手が居なくても目的は果たされている | セッション ID は再起動で意味を失う |
| `CancelValidation` | 同上 | 同上 |
| `RequestHumanApproval` | 画面に出た | `decisions` 側が `idempotency_key` で守る |
| `PersistState` | 保存した | 鍵を持たない (毎回出してよい) |

* **「積めた」を「届いた」にしない。** `queue_submit` が真を返すのは
  配達待ちへ積めたということでしかない。そのあと相手が消えれば
  `Act::Gone`、入力欄が空かないまま上限に達すれば `Act::GaveUp` になる。
  積んだ時点で完了にすると、**どちらでも指示は消える**のに Runtime は
  「送った」と信じたままタスクを抱え続ける (完了した鍵は二度と出ない)。
  結末は `submit_tick` が目印つきで 1 回だけ返す — **送信経路は 1 本のまま**で、
  返しているのは結果だけ
* **同じ logical agent を 2 体起こさない。** 起動が成功してから結び付けが
  保存されるまでの間に落ちると、記録には残らないのにセッションだけが残る
  (Zaivern は自分のセッションを生ログごと復元するので、次の起動でも
  生きている)。`TeamAgent::session_identity` (= 生ログの絶対パス。復元しても
  綴りが変わらない) を覚えておき、起動要求へ `adopt` として載せる。実行側は
  **起こす前に**引き取れるセッションを探す (`launch::adopt_choice` — 純関数)。
  死んでいるもの・既に別の担当へ結び付いているものは選ばない
* **古い結果は採らない。** 実行 ID (`run_id:task:attempt:generation`) が
  いま待っているものと一致しない結果は、記録もせず捨てる。前の試行の
  「成功」が遅れて届いて、新しい試行の証跡を上書きする事故を構造で防ぐ

番人は `team::crash_tests` (落ちる瞬間を `to_saved()` → `restore()` で
決定的に作る) と、実行側の配線を見る `team::wiring_tests`。

### 承認は「その 1 回」にしか効かない

承認は `DecisionKind::ValidationExecution` として既存の approval gate を
通る。**コマンド文字列だけで覚えてはいけない** — エージェントは `build.rs` /
テスト本体 / `Makefile` を書き換えられるので、「同じ `cargo test` だから
承認済み」にすると、人が見て承認したのとは**別のコード**が走る。

記録は `ValidationApproval { run_id, task_id, generation, command }`
(`run.validation_approvals`)。`generation` は**検証回**の番号で、
`begin_validation_round` — 検証回が始まる唯一の場所 — が 1 つ進める。

| 場面 | 前の承認が効くか |
| --- | --- |
| 別のタスクの同じコマンド | **効かない** (`task_id` が違う) |
| 差し戻し後の再検証 | **効かない** (世代が進む) |
| レビュー指摘 → 修正 → 再検証 | **効かない** (同上) |
| 別の Run の同じタスク番号 | **効かない** (`run_id` が違う) |
| Stop で打ち切った検証の再開 | 効く (同じ検証回・コードは変わっていない) |

* 承認要求 (`Decision`) には**その世代を焼き付ける** (`validation_generation`)。
  いまの世代を見て決めると、遅れて届いた承認が新しいコードを通してしまう
* 鍵にも世代を入れる。入れないと「同じ鍵の判断がある」と見なされて聞き直せない
* **拒否したら `NeedsUser`** — 実行しないまま `Validating` で待ち続ける経路は
  無い。拒否のときは前任の保持も解くので、人が Retry を押せば動き出せる

**MVP で自動実行しないもの**: git push / PR 作成 / merge / rebase / reset /
deploy / release / publish / 本番 DB 操作 / credential 操作 / 課金 /
`rm -rf` 等の破壊的操作 / `sudo` 等の権限昇格 / ワークスペース外への
書き込み / 任意の shell 文字列。

### 隔離 (sandbox) は無い

承認した `cargo test` が何をするかは、**リポジトリのコードが決める**。
Zaivern が保証できるのは「何を起動したか」までで、起動したプロセスが
その先で何をするかは保証できない。完全な隔離が要るなら、sandbox か
使い捨てのコンテナで動かすこと。

### 検証は必ず決着する

* **時間切れがある** (既定 10 分 / `run.validation_timeout_secs`)。
  無期限に待つと、そのタスクは永久に `Validating` に残る
* 打ち切りは**プロセスツリーごと** (既存の `procx::kill_tree`)。
  直接の子だけを殺すと孫が残る
* 実行器との接続が切れたら `RunnerDisconnected` として失敗を記録する
  (握り潰すと `validation.running` が true のまま残る)
* 結果も切断も来ない worker は、パネル側の見張りが時限 + 余白で見切る
* 終わり方は `ValidationOutcome` (`Passed` / `Failed` / `TimedOut` /
  `Cancelled` / `SpawnFailed` / `RunnerDisconnected`) として保存され、
  画面と `--json` の両方に出る
* 結果には**実行 ID** (`run_id:task:attempt:generation`) が付く。一致しない
  結果は採らない — 差し戻して配り直した後に古い実行の結果が届いて、
  新しい試行の証跡を上書きするのを防ぐ

### 人が戻せる状態は、必ず配り直せる

`RetryTask` は「その手前で担当を解放済み」を前提にしている。解放し忘れた
経路を足すと、押しても `Ready` のまま動かない
(`PreviousHolderNotStopped` で永久に断られる)。

* 戻せる状態は `NeedsUser` / `Failed` / `Blocked` の 3 つ
* どれも、その手前で `release_after_self_report` を通っていること
* 番人は `runtime_tests::人が戻せる状態はどれも配り直せる` — **状態が
  変わっただけでは合格にしない**。指示が実際に飛ぶところまで見る

### Pause と Stop と Discard

* `Pause` — **新しい仕事を始めない**。新規割り当てに加えて、新しい検証も
  始めない (検証はリポジトリのコードを走らせる「仕事」なので)。走っている
  ものは走り切る。`Resume` で再開する
* `Stop` — 新規割当を即座に止める。**実行中エージェントの kill は承認ゲートを
  通す** (`Decision` を立てて人の承認を待つ)。承認されたら、エージェントと
  **走っている検証のプロセスツリー**の両方を止める
* 止めた Run へ遅れて「成功」が届いても、`accepting_work` が false なので
  先へは進まない
* `Discard` — **先に止めてから消す**。走っている検証へ停止の札を立て
  (worker が次の刻みで木ごと落とす)、結果の受け口は捨てない (捨てると
  Runtime が永久に待つ)。そのあと置き場を消す

## 永続化

```
~/.zaivern/team/<ワークスペースキー>/
  state.json       ← Run 全体 (いまの世代)
  state.prev.json  ← 直前の完全なスナップショット
  launch.json      ← zai team run の投函箱 (拾うと同時に消える)
```

* 置き場は `history::workspace_key` から決める (自前で 16 桁キーを作らない)
* **Run 全体を 1 つのスナップショットとして置き換える。**
  ファイル単体が原子的でも、まとまりとしては原子的ではない — 版 3 までの
  8 ファイル方式では、3 つ目まで書いたところで電源が落ちると新しい
  `run.json` と古い `tasks.json` が同居した。どちらも JSON としては
  正しいので、読む側は正常な状態として読んでしまう

  ```text
  state.json.<pid>.<ns>.tmp  ← 書いて fsync
           ↓ rename          state.json → state.prev.json
           ↓ rename          tmp        → state.json
  ```

* **直前の完全なスナップショットを必ず 1 つ残す。** いまのものが壊れて
  いたら `state.prev.json` へ戻る
* 世代を持つ。どちらが新しいかを **mtime に頼らない** (コピーや同期で
  簡単にひっくり返る)
* 読んだあとに**噛み合い**を見る — タスクの goal が goal と一致するか、
  判断が実在するタスクを指しているか。旧形式の新旧混在は形だけ見ても
  気付けない
* 旧形式 (版 3 以前) も読める。次の保存で 1 ファイルへ移り、旧ファイルは
  `.legacy-<epoch>` へ退く (**2 つの真実を残さない**)
* 保存の途中で落ちる筋書きは `fault_inject` で 3 段を指して作れる。
  番人 `どの段で落ちても新旧が混ざらない` が 3 段すべてを回す
* `Instant` を永続化しない。時刻は全部 Unix 秒
* **壊れていても黙って初期化しない** — `<名前>.corrupt-<epoch>` へ退避して
  理由を返す
* 版が新しすぎるときは読まずに残す
* **状態機械が断った遷移は事象として残す** (`TransitionRejected`)。
  断ること自体は正しい (完了したタスクへ古い報告が来ても動かさない) が、
  黙って無かったことにすると「押したのに何も起きない」を誰も追えない
* 人が承認した検証 (`validation_approvals`) と時間切れ
  (`validation_timeout_secs`) もスナップショットに残る。承認は
  Run + タスク + 世代 + コマンドで縛られているので、**復元しても
  範囲は広がらない**
* **保存ファイルの中の `workspace` は権限を持たない。** 復元時の
  workspace は、いま開いているものだけが決める (書き換えられても
  検証の実行場所は変わらない)
* 事象もスナップショットへ入れる (上限 5,000 件)。別ファイルにすると
  「復元した状態」と「そこまでの経緯」の世代がずれる

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
