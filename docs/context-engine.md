# Context Engine — AI へ渡す前に情報量を最適化する層

```text
Task / State → [Context Engine] → Agent Provider → Execution Target
```

エージェントに渡る文脈は、ほとんどが**読まれない部分**でできている。
2000 行のファイルを丸ごと渡しても、必要なのは 1 つの関数だったりする。
Context Engine はその差を、**渡す前に**削る層。

追加インストールは要らない。`zai` を入れた人はその場で使える
（`cargo install token-slim-mcp` も `claude mcp add …` も不要）。

## 使い方

```console
$ zai context read src/config.rs
[context] read src/config.rs strategy=outline(auto) lines=7022 ~85285→~3369 tok (-96%) [capped]
  — structure only, no bodies: fetch a function with offset/limit, or the whole file with strategy=slim
L53: pub enum BlameMode {
…

$ zai context read src/config.rs --offset 1330 --limit 4
[context] read src/config.rs strategy=slim(auto) lines=7022 range=L1330..1333 ~41→~41 tok (±0%)
pub(crate) fn zaivern_dir() -> PathBuf {
…
```

| コマンド | 何をするか |
|---|---|
| `zai context read <パス>` | ファイルを畳んで出す（既定 `auto`） |
| `zai context grep <正規表現>` | 木を検索して `path:line:本文` だけを出す |
| `zai context refs <記号>` | 参照を definition / call / test / import / comment / mention へ分類 |
| `zai context map [<場所>]` | ディレクトリの地図（`ls -R` / `find` の代わり） |
| `zai context json <パス>` | JSON / JSONC を最小化して刈る |
| `zai context text <パス>` | ログや出力を畳む（`-` で標準入力） |
| `zai context tokens <パス>` | トークン数を見積もる（**何も畳まない**） |
| `zai context stats` | これまでの削減量 |

GUI からはコマンドパレットの「🧠 コンテキストエンジン」。

## 4 つの畳み方 (`--mode` / `context.mode`)

| 戦略 | 何を返すか |
|---|---|
| `auto`（既定） | 大きくて構造のあるファイルは `outline`、それ以外は `slim` |
| `slim` | コメントを外し、空行を畳む |
| `outline` | 構造の行だけ（関数 / クラス / 見出し + 行番号） |
| `raw` | そのまま |

**行域（`--offset` / `--limit`）を指定したときは決して outline しない。**
行域の指定は「outline を見て、次にここを読む」という手順の 2 歩目そのもので、
そこで構造だけ返すと永久に本文へ辿り着けない。

## 設定

`~/.zaivern/config.toml` の `[features]`（設定画面の「機能」からも変えられる）:

```toml
[features]
"context.enabled" = true          # 呼ばれたときにだけ働く
"context.mode" = "auto"           # auto | slim | outline | raw
"context.max_tokens" = 4000       # 0 で上限なし
"context.max_results" = 50        # 検索・参照の一覧の上限
"context.persist_metrics" = false # 削減量を ~/.zaivern へ残すか
```

`enabled` の既定が `true` なのは、この層が**明示的に呼ばれたときにしか
動かない**から。既存の挙動は 1 つも変わらない。

## 勝手に何もしない

Context Engine は**呼ばれたときにだけ**動く。次のことは**しない**:

* エージェントへ文字を打ち込む / Enter を送る
* プロンプトを書き換える
* 利用者のファイルを書き換える
* 外部と通信する

読むのはワークスペースの中だけで、そこは型が強制する（下記）。

## Provider に依存しないこと

`ClaudeContextOptimizer` / `GeminiContextOptimizer` のような形にすると、
エージェントが 1 つ増えるたびにこの層が増える。それは基盤ではない。

ここは**入力の内容だけ**を見て畳み方を決め、どのエージェント向けかは
`ContextOrigin { agent, session, task }` という**分類のラベル**としてしか
受け取らない。言葉ではなく番人で守っている:

* `context::tests::コアはエージェント名を知らない` — `src/context/` の
  製品コード（コメントと `#[cfg(test)]` 以降は除く）を走査して、
  `agents::AGENT_CATALOG` の実行ファイル名が出てこないことを確かめる。
  **わざと分岐を仕込んで赤になることを、同じテストの中で先に証明する**
* `context::engine::tests::出自は挙動を変えない` — 同じ入力を 6 通りの
  出自で流して、結果が 1 バイトも変わらないことを確かめる

## MCP と圧縮ロジックを分けてあること

元にした [token-slim-mcp](https://github.com/tacyan/claude-code-token-slim-mcp)
は `LLM → MCP → token-slim` という積み方だった。ここでは
`Zaivern → Context Engine → 圧縮` に組み替えてあり、
**JSON-RPC / stdio / `tools/call` の包みはコアに 1 バイトも無い**。

```text
Context Engine (src/context/)
 ├── Zaivern 内部 API   … crate::context::ContextEngine
 ├── CLI アダプタ       … src/context/cli.rs   (`zai context …`)
 ├── UI アダプタ        … src/context/panel.rs (パレット → 窓)
 └── MCP アダプタ       … 未実装 (コアを触らずに足せる)
```

コアが知らないもの: 通信・エージェント・上限の出どころ（上限は
`ContextLimits` として**渡される**。環境変数も設定ファイルも読まない）。

## API

```rust
use crate::context::{ContextEngine, ContextRequest, ContextSource, ContextStrategy, Workspace};

let engine = ContextEngine::new(Workspace::new(&roots)?).with_limits(limits);

// 同期 (CLI・テスト)
let out = engine.run(&ContextRequest::new(ContextSource::File {
    path: "src/app.rs".into(),
    params: Default::default(),
}).with_strategy(ContextStrategy::Auto))?;

out.render();                        // ヘッダ + 本文 (実際に渡す形)
out.metrics.original_tokens;         // 素直にやったときのトークン数
out.metrics.optimized_tokens;        // 渡すトークン数
out.metrics.reduction_percent();     // 削減率
out.metrics.elapsed_ms;              // かかった時間

// 非同期 (描画スレッドから呼ぶのはこちら)
let rx = engine.spawn(req, move || ctx.request_repaint());
```

`Workspace::new` は根が空だと作れない（空にすると「どこでも読める」と
同じ意味になる）。以後、道具は**境界検査を通った `SafePath` しか受け取らない**
ので、検査を忘れたまま `fs::read` へ流す書き方が構造的にできない。

境界検査は 4 段: 相対パスを根から解決 → `..` を**字句で**畳む（実体が無い
パスでも判定できる）→ 実体があれば canonicalize（symlink 越しの脱出も塞ぐ）
→ どの根にも収まらなければ `ContextError::OutsideWorkspace`。
走査は symlink を辿らない。

## メトリクス

1 回ごとの値は `ContextMetrics`（`origin` の agent / session / task を含む）
として呼び出し側へ返る。プロセス内の集計は常に取っていて、
`context.persist_metrics = true` のときだけ
`~/.zaivern/context/metrics.json` へも残る。

残すのは**日ごと・操作ごと・エージェントごとの合計だけ**。1 回ごとの明細も、
読んだ内容も、パスも保存しない。日数は 400 日、エージェント名は 32 件で
打ち切るので、ファイルは**構造的に有界**（Multi Cockpit の
「Context saved today」はこの値から出せる）。

## ベンチマーク

`tools/context-bench.sh [<token-slim-mcp の実行ファイル>]` が、同じ入力を
両方へ流して `original / optimized / reduction% / reps / total_s` を並べる。
実測（2026-08-26, release ビルド, Linux x86_64, `REPS=100`）:

| 入力 | token-slim | Zaivern |
|---|---|---|
| 400 関数の Rust (`auto`) | 39019 → 3275 (-91%) | 39019 → 3272 (-91%) |
| 同 (`slim`) | 39019 → 3093 (-92%) | 39019 → 3090 (-92%) |
| 同 (`outline`) | 39019 → 3275 (-91%) | 39019 → 3272 (-91%) |
| 2000 件の JSON | 169950 → 1269 (-99%) | 169950 → 1269 (-99%) |
| 4000 行のログ (`aggressive`) | 34020 → 128 (-99%) | 34020 → 128 (-99%) |

出力の中身は**打ち切りの印の文言以外バイト単位で同じ**だった（3 トークンの
差はその文言の長さ）。

時間はどの段も**両側とも 100 回で 0〜1 秒**で、この分解能では差が出ない。
per-op の ms を出さないのは、**POSIX に秒未満を測る移植性のある手段が無い**
ため（`date +%s%N` は GNU 限定、`time` キーワードは bash 限定。最初 bash で
書いて、CI の `sh -n` に落とされた）。両側を同じ回数・同じ測り方で回した
**粗い比較**として読むこと。秒にはプロセス起動が入る。

この比較は外部リポジトリが要るので CI では回せない — だから床だけを
`context::tests::削減率は代表入力で床を下回らない` に固定してある。

## 将来 core crate として切り出すときの継ぎ目

`crate::` を参照しているのは**コア側で 3 か所だけ**。いずれも小さな純関数:

| 参照先 | 使う場所 | 代わりに要るもの |
|---|---|---|
| `crate::pathx` | `walk.rs` | `..` の畳み込みと canonicalize |
| `crate::worktree::fs_case_insensitive_at` | `walk.rs` | 大小非区別の実 FS 検査 |
| `crate::jsonc::strip_jsonc` | `tools/json.rs` | JSONC の正規化 |

`cli.rs` / `panel.rs` / `mod.rs` の `FEATURE` は**アダプタ**なので対象外。

## token-slim-mcp から何を持ってきたか

| 元 | 扱い |
|---|---|
| `slim.rs`（コメント除去・空行畳み・outline・JSON 刈り） | ほぼそのまま `optimizer.rs` へ |
| `refs.rs`（参照分類・スコープ解決） | ほぼそのまま `tools/refs.rs` へ |
| `glob.rs` | そのまま `glob.rs` へ |
| `tokens.rs` | `metrics.rs` へ（台帳を足した） |
| `tools.rs` の走査 | `walk.rs` へ。**ワークスペース境界の検査を足した** |
| `tools.rs` の `env::var("TOKEN_SLIM_…")` | `ContextLimits` へ（設定はアダプタが渡す） |
| `main.rs`（JSON-RPC / stdio）・`tool_definitions()`・`Value` からの引数抽出 | **持ってきていない** |

`strip_jsonc` は既にリポジトリにあった `crate::jsonc::strip_jsonc` へ寄せた
（同じ仕事の実装を 2 つ持つと必ずずれる）。
