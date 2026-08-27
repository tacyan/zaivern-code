# Cloud Execution Provider — 計算資源を差し替え可能にする層

> **これは「Hetzner 対応」ではない。** どのマシン・どのクラウド・どの AI
> エージェントでも使える **Execution abstraction** を 1 枚足す仕事である。
> Hetzner はその抽象が本当に抽象になっているかを確かめるための**最初の実装**。

```text
Task / State → Context Engine → Agent Provider → [Cloud Execution] → 実行先
                                                       ├─ Scheduler
                                                       ├─ Execution Provider
                                                       ├─ Execution Target
                                                       └─ Execution Transport
```

---

## 1. アーキテクチャ

### 1-1. 3 つを混ぜない

| 層 | 責務 | v1 の実装 | 置き場 |
|---|---|---|---|
| **Provider** | 実行先を**どこから持ってくるか**だけ | Local / StaticSsh / Hetzner | `provider/` |
| **Transport** | そこで**どう走らせるか**だけ | Local / Ssh | `transport/` |
| **Scheduler** | **どれを選ぶか**だけ (純関数) | 能力 → 空き → 費用 | `scheduler.rs` |

**`HetznerProvider` はコマンドを 1 つも実行しない。** VM を作る・数える・消す・
`ExecutionTarget` へ変換する、までが仕事で、走らせるのは `SshTransport`。
その `SshTransport` は Hetzner を 1 バイトも知らない。

だからこうなる:

* Hetzner で作った VM も、自宅の Linux も、**同じ Transport** で動く
* Provider を 1 つ足しても、Scheduler と Transport は**1 行も変わらない**
* API を持たない VPS でも、`IP` + `SSH` + `Linux` があれば実行先になる

最後の行が v1.0 最大の価値である (Contabo / OVH / netcup / Hostinger /
Oracle / 会社のサーバー / 自宅の Linux — Provider 固有の実装は要らない)。

### 1-2. なぜ Provider と Transport を分けるのか

分けないと、クラウドが N 個・接続方式が M 個で **N×M 個の実装**になる。
分けてあれば **N+M**。実際 v1 は Provider 3 + Transport 2 = 5 個の実装で
「手元 / 任意の SSH / Hetzner」の 3 通りを賄っている。

もう 1 つの理由は**責任の切れ目**。「VM が作れない」(課金・API・権限) と
「コマンドが失敗した」(鍵・ネットワーク・リモートの状態) は、直し方が
まったく違う。層が同じだと、エラーがどちらの話かを利用者に言えない。

### 1-3. ファイル

```text
src/features/cloud_execution.rs        登録 (FEATURE) と設定
src/features/cloud_execution/
├── model.rs          共通語彙 (Target / Capabilities / Requirements / Error)
├── scheduler.rs      どれを選ぶか (純関数。I/O ゼロ)
├── registry.rs       台帳 (誰が居て、いま何本走っているか)
├── store.rs          ~/.zaivern/cloud/ の置き場と atomic な置き換え
├── redact.rs         秘密を伏せる場所 (1 か所)
├── command.rs        LaunchSpec → 実行先の上のコマンド行
├── git_workspace.rs  リモート bare リポジトリ / worktree / 結果の持ち帰り
├── runner.rs         用意 → 実行 → 持ち帰り → 片付け
├── cli.rs            `zai cloud …`
├── panel.rs          パレットから開く窓
├── transport/{mod,local,ssh}.rs
├── provider/{mod,local,static_ssh,hetzner,http}.rs
└── test_support.rs   FakeProvider / FakeTransport / FakeHttpClient
```

`src/main.rs` / `src/palette.rs` / `src/feature.rs` は 1 バイトも触っていない
(feature registry が `src/features/*.rs` の走査で自動登録する)。

---

## 2. Static SSH — API を持たないマシンを使う

```bash
zai cloud target add ssh \
  --name dev-01 \
  --host example.com \
  --user zaivern \
  --port 22 \
  --max-jobs 4

zai cloud target trust dev-01        # 相手の鍵の指紋を見る (まだ記録しない)
zai cloud target trust dev-01 --yes  # 見比べて同じなら記録する
zai cloud target probe dev-01        # 届くか・何者かを確かめる
zai cloud exec --target dev-01 -- uname -a
```

### なぜ `trust` が要るのか

最初の接続では**必ず**「この鍵を知らない」になる。ここで自動的に受け入れる
(`StrictHostKeyChecking=accept-new`) と、**その 1 回だけは中間者を検出できない**。
かといって何も用意しないと、登録した実行先へ永久に繋がらない
(実際に最初の版がそうなっていて、実機で確かめて分かった)。

だから 3 段にする: **取ってくる → 指紋を見せる → 人が同意したら記録する**。
`--yes` が無ければ見せるだけで止まる。すでに**違う鍵**を記録している相手は
黙って上書きせず、断る (機械を作り直したのでなければ、中間者攻撃と区別が
付かないため)。

`probe` は `uname` / CPU / メモリ / ディスク / シェル / 道具 (`git` 等) を
読んで台帳へ書き戻す。**`probe` が通るまで実行先は `Ready` にならない** ので、
届かない機械が Scheduler へ渡ることはない。

### 同時実行枠

**1 台 = 1 エージェントに固定しない。** 安い大きめの VPS 1 台に

```text
VM
├─ worktree A → エージェント A
├─ worktree B → エージェント B
├─ worktree C → テスト
└─ worktree D → レビュー
```

を載せられる。枠は `--max-jobs` で**利用者が決める** (CPU から自動推論しない)。
枠の増減はロックの中で確かめてから足すので、複数インスタンスが同時に
取りに行っても上限を超えない。

---

## 3. Hetzner — API で Worker を作る

```bash
export HCLOUD_TOKEN="…"          # 値は Zaivern のどこにも保存されない

zai cloud provider add hetzner \
  --name hetzner-eu \
  --location fsn1 \
  --server-type cx33 \
  --image ubuntu-24.04 \
  --ssh-key zaivern            # Hetzner 側に登録済みの**公開鍵の名前**

zai cloud provider types hetzner-eu    # 種別と、その時点の費用 (API から取得)
zai cloud worker create --provider hetzner-eu --name zai-worker-01 --wait
```

### 状態の進み方

```text
Requested → Provisioning → Running → SSH Waiting → Ready
```

**`status: running` でも `Ready` にしない。** OS が起動していても SSH が
まだ開いていないことがあり、そこへ仕事を載せると必ず失敗する。
`Ready` を名乗れるのは、実際に接続を確かめた `probe` だけ。

`--wait` は SSH が開くまで待つ (上限 5 分。永久には待たない)。

### 破棄と実行の排他 (`destroying`)

**「実行中 0 件を確かめてから消す」だけでは足りない。** Provider への往復は
数秒あるので、そのあいだに別のプロセスが枠を取って仕事を載せられる:

```text
A: active_jobs == 0 を確認
                        B: 枠を取って仕事を載せる
A: VM を消す                ← 走っている仕事ごと消える
```

だから **「確認」と「削除中への遷移」を台帳ロックの中で原子的に**行い、
そのあと**ロックを手放してから** Provider を呼ぶ。枠取り (`claim_slot`) は
同じロックの中で台帳を読み直すので、遷移後の要求は必ず断られる —
**呼び出し側が古い `ExecutionTarget` を握っていても同じ**。

`ExecutionTarget` のライフサイクルに `destroying` が加わる
(第 2 の状態機械は作らない):

| 状態 | 枠を配るか | 意味 |
|---|---|---|
| `ready` | **配る (これだけ)** | 接続を確かめた |
| `destroying` | 配らない | 破棄を予約した / 結果が分からない |
| `stopped` | 配らない | 止まっている・消えた |
| `unknown` / `provisioning` / `draining` / `failed` | 配らない | まだ確かめていない・抜けかけ・故障 |

**落ちた仕事の枠は、次の起動で返る。** 枠を返すのは `SlotGuard` の Drop だが、
`kill -9` / OOM Killer / 電源断では Drop が呼ばれず、台帳に「走っている 1 本」が
残る。`registry::reconcile_active_jobs` が `Registry::load` のたびに 2 段で片付ける:

1. 仕事の台帳のロックの中で、**持ち主の PID がもう居ない**未完了の記録だけを
   `failed` へ移す (`ExecutionJob::owner_pid`)
2. 実行先の台帳のロックの中で、**その本数だけ引く**

**引き算しかしない**のが要で、1 と 2 のあいだに別のプロセスが枠を取っても
その枠は消えない。だから後始末が `max_jobs` を超えさせることも、生きている
仕事の枠を奪うこともない。判定は PID の生存だけなので、PID が再利用された
ときは枠が返らないが、再利用した側が終われば次の後始末で返る。

**枠を配るのは `ready` だけ。** Scheduler も同じ判定 (`lifecycle == Ready`) を
するが、あちらが見るのは*選んだ瞬間の写し*でしかない。選んでから載せるまでの
あいだに `probe` の失敗や `destroy` の予約が入りうるので、
**最後にもう一度、台帳ロックの中で確かめる**のが `claim_slot`。
Scheduler は説明つきで「どれが良いか」を選ぶ純関数のまま、
「いま本当に載せてよいか」の 1 点だけがロックの中にある。

**走っている仕事がある実行先は一覧から外せない** (`remove_target`)。外すと
`SlotGuard` の返す先が消えて、記録だけが孤児になる。`destroy` と同じく、
名前を引くのも本数を数えるのも消すのも**同じ台帳ロックの中**で行う。

### 破棄が失敗したとき (回復方針)

| 失敗 | 台帳の扱い | 理由 |
|---|---|---|
| 認証・設定・安全のための拒否 | **元の状態へ戻す** | `DELETE` を送る前に止まっている = サーバーは消えていない |
| 時間切れ・通信・Provider の 5xx | **`destroying` のまま留める** | 届いたか分からない。`ready` へ戻すと消えかけの機械へ次の仕事を載せる |
| プロセスが途中で死んだ | `destroying` のまま残る | 同上 |

**`destroying` から戻す道は 2 つ:**

```bash
zai cloud target probe <名前>    # Provider / SSH の現状を確かめ直す
                                 #   届く → ready へ戻る
                                 #   届かない → failed (消えていた)
zai cloud target remove <名前>   # 台帳から外すだけ (機械には触らない)
```

**自動では戻さない。** 「たぶん消えていないだろう」で `ready` に戻すのは、
いちばんやってはいけない推測である。

### 消せるサーバー

Zaivern が作ったサーバーには label が付く:

```text
managed_by=zaivern
zaivern_target_id=<実行先 ID>
zaivern_profile=<プロファイル名>
```

**印が無ければ消さない (fail closed)。** しかも手元の台帳の `managed` だけを
信じない — 消す直前に Provider へ問い合わせ、**向こうの label** を確かめてから
`DELETE` する (手元の台帳はテキストファイルで、編集できてしまうため)。

**2 つの印は両方とも必須。** `managed_by` だけが付いていて
`zaivern_target_id` が無い / 空 / 食い違うサーバーは、どれも消さない
(「有れば照合する」にすると、印を失ったサーバーが素通りする)。

---

## 4. セキュリティ

### 4-1. host key

* **`StrictHostKeyChecking=no` を書かない。** 型として持たず、ソースの走査でも
  禁じている (`ssh_never_disables_host_key_checking`)
* known_hosts は Zaivern 専用: `~/.zaivern/cloud/known_hosts`
  (利用者の `~/.ssh/known_hosts` を汚さない)
* **作りたての VM の初回だけ** `accept-new`。1 度成功すれば以後は strict

作り直した機械へ繋ぐと「ホスト鍵が違う」と断られる。**それが正しい** —
中間者と区別が付かないので、該当行を消すのは人の判断でなければならない。

### 4-2. コマンド注入

`ssh` の呼び出しは**必ず引数配列**で行う (`sh -c "ssh …"` としない)。
接続情報も構造化して持ち、`ssh_opts: String` のような任意文字列の欄を
作らない。

ただし **OpenSSH はリモート側のコマンドを必ず 1 本の文字列としてリモートの
シェルへ渡す** (これは仕様で、こちらでは変えられない)。だから境界に 1 つだけ
「リモートのシェル向けに引用する」処理が要る。それが `posix_quote` で、
入口は `remote_script` の 1 か所だけ。表で固定してある:

```text
空白 / ' / " / ; / && / || / | / $ / ${} / ` / 改行 / タブ / -rf / --host / * / ~ / \ / 絵文字
```

unix では**実際の `sh` へ通して 1 バイトも変わらずに戻ること**まで確かめる。

ホスト名とユーザー名は `-` 始まりを断る (`-oProxyCommand=…` をホスト名として
渡されると任意コマンドが走るため)。

**リモートのパスは使ってよい文字を数え上げる** (`RemotePath`)。英数字と
`/._-+=@,~` だけを通し、外れたら拒否する (`~` は先頭だけ)。`scp` のリモート側の
パスは OpenSSH 9.0 より前では*リモートのシェル越し*に渡るので、
`/tmp/a;touch /tmp/pwned` や `/tmp/$(id)` が転送のつもりで**実行される**。
引用符で包む案は採らない — 9.0 以降の scp は SFTP を使うため
`host:'/tmp/a b'` の引用符が*名前の一部*になり、**同じ入力が版で別の物を指す**。
狭めて拒否するほうが、どの版でも意味が 1 つに決まる。

### 4-3. 秘密

保存してよいもの / 禁止:

| 保存してよい | 保存しない |
|---|---|
| Provider 名・種別・location・server type・image | API トークンの**値** |
| SSH 鍵の**名前**、秘密鍵の**パス** | 秘密鍵の**中身**、パスワード |
| トークンを持つ**環境変数の名前** | Claude / OpenAI / GitHub の資格情報 |

* 伏せ方は `redact.rs` の 1 か所 (`Authorization` / `Bearer` / 環境変数の値 /
  PEM の本文 / `token=` `access_token=` `api_key=` `apikey=` `password=` `secret=`)
* **仕事の記録も同じ 1 か所を通る。** `jobs.json` へ書くコマンド行は
  `LaunchSpec::safe_display()` (= `redact`) で伏せてから書き、
  `zai cloud job list` は読むときにもう一度通す (伏せる規則が増える前に
  書かれた記録が残っているため)。`curl -H 'Authorization: Bearer …'` を
  1 度走らせただけで、記録・バックアップ・画面共有の全部に生のトークンが
  残るのがこの穴だった
* `CloudError` は `Debug` を導出しない (欄を足した日に静かに漏れるため)。
  `Debug` も `Display` も同じ経路を通る
* トークンを**引数で受け取らない** (`--token` は拒否する。履歴と `ps` に残る)
* 保存の直前に `assert_no_secret()` が形を確かめる。「保存しないつもり」ではなく
  保存する経路そのもので止める

### 4-4. エージェントの資格情報 (§24)

**Cloud Execution はエージェントの資格情報を運ばない。** リモートで
エージェントに login 済みであること、または利用者が
`zai cloud shell` から login することを前提にする。
Credential Broker は将来仕様。

名前から秘密と分かる環境変数 (`*TOKEN*` / `*SECRET*` / `*PASSWORD*` /
`*API_KEY*` / `*CREDENTIAL*`) は、転送する環境変数から**自動で外れる**。

---

## 5. リモートの Git モデル

**rsync だけに依存しない。** 向こうに履歴が無いと、誰が何を変えたのかを
持ち帰れない。かといって GitHub の PAT や deploy key をリモートへ配りたくない。

```text
手元のリポジトリ
     │ git push over SSH   (Zaivern が持っている SSH 鍵だけを使う)
     ▼
リモートの bare リポジトリ  ~/.zaivern/cloud/repos/<ワークスペースキー>.git
     ├── worktree ~/.zaivern/cloud/jobs/<job>   ブランチ zai/cloud/<job>
     ├── worktree …
     └── …
```

**送るのは `HEAD` だけ。** 未コミットの変更は 1 バイトも向こうへ行かないので、
`zai cloud job run` は作業ツリーが汚れていたら**始める前に断る**
(`git_workspace::ensure_clean_worktree`)。追跡中の変更・index に載せた変更・
追跡していないファイルのどれでも断り、`.gitignore` されたものは数えない
(`--untracked-files=all` を明示するので、`status.showUntrackedFiles=no` を
global に置いている利用者でも穴が開かない)。黙って進むと「いまの作業ツリーで
走った」と読まれたまま**最後のコミット**が走り、その食い違いはどの出力にも
現れない。v1 では自動 snapshot は取らない。

リモートの bare リポジトリの用意は **`mkdir` を鍵にして 1 本へ絞る**
(4 本同時で `could not lock config file` が実測で出た)。`kill -9` や電源断では
`trap` が発火せず鍵が残るので、**10 分より古い鍵は持ち主が死んだものとして
回収する** (`git init --bare` は 1 秒とかからない)。鍵を取った直後に `owner` を
書くので、たったいま取られた鍵が奪われることはない。

1. `git push <ssh-url> HEAD:refs/zaivern/base/<job>` — 手元の HEAD を送る
2. `git worktree add -B zai/cloud/<job> <job の置き場> refs/zaivern/base/<job>`
3. リモートでコマンド／エージェントが走る (**仕事ごとに別の worktree**)
4. 終わったら `git status --porcelain` を見て、未コミットがあれば
   **輸送用コミット**を作る (`zaivern: snapshot cloud job <job>`)
5. **そのときの `HEAD` を、リモート側の動かない参照へ固定する**
   (`refs/zaivern/result/<job>`)
6. 手元へ `git fetch <ssh-url> +refs/zaivern/result/<job>:refs/remotes/zaivern-cloud/<job>`
7. **リモートで確定した OID と、手元に届いた OID が一致することを確かめる**
8. **一致したときだけ** worktree を片付ける

### なぜ枝ではなく参照へ固定するのか

snapshot が付くのは**枝ではなく `HEAD`** である。エージェントが
`git switch -c` で別の枝へ移ったり、detached HEAD のまま作業すると、
成果は `HEAD` 側に付き、作った時の枝 `zai/cloud/<job>` は**古いまま**残る。

その枝は実在するので **`fetch` は成功する**。つまり、1 バイトも持ち帰らない
まま「成功」として worktree を片付けてしまう — **作業が消えるのに、
どこにもエラーが出ない**。いちばん静かな壊れ方である。

だから「HEAD を動かない参照へ固定 → その参照を取る → OID を突き合わせる」
の 3 段にしてある。OID の長さは決め打たない (SHA-256 のリポジトリでは 64 桁)。

```bash
zai cloud job run --target dev-01 -- cargo test --workspace

git log refs/remotes/zaivern-cloud/<job>
git diff HEAD..refs/remotes/zaivern-cloud/<job>
```

### 触らないもの

**利用者の枝には一切触らない。** `merge` / `rebase` / `cherry-pick` /
`reset` / `push origin` を 1 度も呼ばない (ソースの走査で固定している)。
持ち帰りは `refs/remotes/zaivern-cloud/<job>` に置くだけで、統合は
既存の Coordinator / review / merge 層の責任。

### 片付けない場合

**結果を持ち帰れなかったら worktree を消さない。** ディスクの節約より
データを失わないほうが大事。

---

## 6. 費用のモデル

`BillingModel` は型だけ最初から持つ (`Free` / `FixedMonthly` /
`HourlyWithMonthlyCap` / `UsageBased` / `Unknown`)。

* **価格表をコードへ埋め込まない。** `CX33 = €8.49` のような行を 1 つでも
  書くと、値上げの日に Zaivern が嘘をつく
* 取れるなら Provider の API から取る。取れなければ `Unknown`
* 通貨も応答から取る (決め打つと、別通貨の請求先で嘘になる)
* **「不明」を「ただ」として扱わない。** 並べ替えの費用ヒントでは真ん中に
  置く — 0 にすると、いちばん高いものを最安として選びかねない

**自動 Provision は既定で OFF。** `--target auto` は「いま Ready な実行先から
選ぶ」だけで、有料 VM を勝手に作らない。作るには
`zai cloud worker create` の明示操作が要る。

---

## 7. Scheduler

```rust
pub fn select_target(
    requirements: &ExecutionRequirements,
    targets: &[ExecutionTarget],
) -> Option<TargetId>
```

**純関数。Provider API を 1 度も呼ばない。** 呼ぶと (1) 同じ入力で同じ結果を
返さなくなり (2) 選ぶだけの操作がネットワークの都合で数秒止まり
(3) Provider が増えるたびに Scheduler が増える。

順序:

1. `lifecycle == Ready`
2. 能力を満たす (OS / arch / CPU / メモリ / GPU / 道具 / 札)
3. 空き枠がある (`active_jobs < max_jobs`) — **手元 (`local`) も数える。
   上限は `default_max_jobs`**
4. 利用者が名指しした実行先
5. ローカル / リモートの好み
6. 費用の目安
7. **同点なら ID 順** (ここまで来ても必ず 1 つに決まる)

「分からない能力」は、要求があるなら**通さない** (fail closed)。
「分からない = 何でもできる」にすると必ず落ちる。

選べなかったときは**理由を実行先ごとに言う**。「空きがありません」だけだと、
利用者は VM を増やして直そうとする — 実際には RAM 不足かもしれない。

```bash
zai cloud exec --target auto --min-memory-mib 8192 --tool git -- cargo test
```

---

## 8. Agent と Context Engine からの独立

### Agent

**Cloud のコアはエージェントの名前を 1 つも知らない** (番人テストが
`AGENT_CATALOG` から名前を起こしてソースを走査する)。起動する中身は
`LaunchSpec` として**外から**渡ってくる。既存の `crate::agents` カタログが
唯一の出所で、こちらへ写経しない。

リモート起動は「コマンド行の書き換え」だけで済ませる:

```text
<元のコマンド行>  →  ssh <実行先> '<元のコマンド行>'
```

```bash
zai cloud launch --target dev-01 --command "<エージェントの起動行>" --cwd '~/work'
```

が返す 1 行を、エージェント設定 (`config.toml` の preset) の `command` に
貼れば、**既存の PTY セッションと Supervisor がそのまま見張る**。
Cloud 専用の端末パーサも、第 2 の Agent 状態機械も作らない
(Cloud が持つのは `ExecutionJobState` = 基盤の状態だけ)。

### Context Engine

**Cloud のコアは `crate::context` を 1 度も呼ばない** (これも番人テスト)。
呼ぶのは上位の Orchestrator で、Cloud へ届くのは**もう出来上がった**
`LaunchSpec` だけ。こうしておくと Context Engine / Agent Provider /
Execution Provider を独立に交換できる。

---

## 9. 置き場

```text
~/.zaivern/cloud/
├── providers.json   Provider プロファイル (秘密は入らない)
├── targets.json     実行先と、いま何本走っているか (手元の枠もここで数える)
├── jobs.json        仕事の記録 (直近 500 件。コマンドは伏せてから書く)
└── known_hosts      Zaivern 専用
```

書き込みは **tmp へ書く → fsync → rename**。書きかけの JSON を残さない。
読んで直して書く操作 (枠の増減) はロックファイルで直列化する
(Windows の *delete pending* は取り合いとして待つ)。

**手元の実行先も同じ台帳で数える。** `local` は設定から毎回組み直すので
一覧としては保存しないが、*いま何本走っているか*だけは `targets.json` の
行として置く — `zai cloud exec` を 2 つの端末から叩けばそれは 2 プロセスで、
プロセスの中の数え上げでは合わないため。新しい置き場も常駐も作らず、
遠隔の実行先とまったく同じロックを通す。

**壊れた状態ファイルを 0 件と言わない。** `zai cloud doctor` は
`providers.json` / `targets.json` / `jobs.json` を段ごとに読み、読めなければ
件数の代わりに理由を出して**不合格 (終了コード 1)** になる。
`--json` は段ごとに `status` (`ok` / `error`) と `error` を持ち、
全体の合否は `ok` に出る。読めていないことを「まだ登録していない」と
取り違えると、利用者は登録し直して**読めていないだけの中身を上書きして失う**。

---

## 10. 既存の remote-host プラグインとの関係

`assets/plugins/remote-host/` は **Cloud Execution の PoC** として扱う。

* **消さない** (既存の利用者を壊すため)
* **新機能を足さない** — SSH の正準実装は Rust 側 (`transport/ssh.rs`)
* プラグインの `ssh_opts` はシェル行へ連結され、host key の確認も
  known_hosts の指定も無い。Rust 側はどちらも直してある
* 対応関係:

  | プラグイン | Rust 側 |
  |---|---|
  | `exec.sh` | `zai cloud exec` |
  | `push.sh` / `pull.sh` | `zai cloud copy` |
  | `agent.sh` | `zai cloud launch` |
  | `worktree.sh` | `zai cloud job run` (git worktree ごと) |

---

## 11. トラブルシューティング

| 症状 | 見るところ |
|---|---|
| `zai cloud target list` に何も出ない | `local` は必ず 1 行出る。出ないなら置き場 (`zai cloud doctor`) |
| `probe` が「この相手の鍵をまだ知りません」 | 最初の接続では必ずこうなる。`zai cloud target trust <名前>` で指紋を見て、合っていれば `--yes` で記録する |
| `probe` が「ホスト鍵が既知のものと違います」 | 作り直した機械なら `~/.zaivern/cloud/known_hosts` の該当行を消す。**心当たりが無ければ繋がない** |
| `probe` が「鍵で認証できませんでした」 | `ssh-agent` に鍵があるか、`--identity-file` を指定したか |
| `exec` が「条件に合う実行先がありません」 | 出力に実行先ごとの理由が出る。`まだ届くことを確かめていません` なら `probe` |
| `worker create` が 401 | `HCLOUD_TOKEN` が未設定か無効。値は表示されない (`zai cloud doctor` で「設定あり / 未設定」だけ分かる) |
| `worker create` が 429 | レートに当たっている。**再試行は自動** (上限つき指数バックオフ、最大 4 回) |
| `job run` が「git リポジトリではありません」 | `job run` は手元のリポジトリの中でだけ動く (HEAD を送るため) |
| 仕事が終わったのに枠が減らない | `zai cloud doctor` が「終わっていない仕事」を数える |

---

## 12. v1.0 でやっていないこと (正直に)

* **GUI から直接リモートのエージェントを起動する導線は無い。**
  `zai cloud launch` が返す 1 行をエージェント設定へ貼る形までが v1。
  UI からは実行先の一覧・確認・Worker の作成と破棄ができる
* AWS / GCP / Azure / OVH / Contabo / Hostinger / netcup / Vultr /
  DigitalOcean / Coder / MimicPC — **Provider を 1 つ足すだけ**で入る形には
  なっているが、v1 には入っていない
* GPU スケジューリング / Kubernetes / Docker スケジューラ
* 完全な autoscaling / Spot / 課金最適化
* 自動 PR マージ (統合は既存の層の責任)
* Credential Broker (§24)
* Windows / macOS を**実行先**として使うこと (クライアントは 3 OS で動く)

---

## 13. 実物での確認 (Live E2E)

CI では実 VM を作らない。本物の Hetzner を触るのは**明示的に指定したとき
だけ**:

```bash
ZAIVERN_CLOUD_E2E=1 HCLOUD_TOKEN=… cargo test cloud_execution -- --ignored
```

どちらかが無ければ飛ばす。ふつうの `cargo test` で課金が起きることはない。
