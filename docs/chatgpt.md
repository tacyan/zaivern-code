# Using Zaivern from ChatGPT

目的は、通常の ChatGPT **Chat conversation** から Zaivern に開発作業を依頼することです。
独自 Chat UI、Codex 専用フロントエンド、ChatGPT Work への切り替えは実装しません。

## 起動と workspace の許可

```sh
zai mcp serve --workspace /absolute/path/to/project --image sha256:IMAGE_ID
```

`--workspace` はサーバ所有者が起動時に許可する **1つのディレクトリ**です。
tool 引数で別のディレクトリや親ディレクトリへ権限を広げることはできません。
`--image` は事前に導入した信頼できる不変の Docker image ID、または
`name@sha256:DIGEST` を指定します。サーバは image を pull しません。
macOS / Linux と稼働中のローカル Unix socket の Docker daemon が必要です。
リモート Docker context / TCP endpoint は拒否します。
Windows は安全なファイルハンドル実装が未対応のため、明示的なエラーで拒否します。

### Agent image の契約

現在は既存 `agents::ACP_CATALOG` の Qwen Code (`qwen --acp`) を使います。
image の通常の起動処理は、コンテナ内で OpenAI 互換のローカル推論サーバを起動し、
ACP を `docker exec` できる間、生存している必要があります。
Qwen とモデル重み・推論エンジン・GNU tar・ビルド用ツールチェーンは事前に image に含めます。
ホストにも tar が必要です。
image に実際の API キー、ログイントークン、秘密鍵を含めないでください。
`HOME=/tmp/agent-home` となり、workspace と一時ディレクトリだけが書き込み可能です。

コンテナは **network=none** です。ホストの Ollama、外部 API、認証付きクラウドモデルには
接続できません。同じコンテナ内の loopback 接続だけを利用します。
Qwen の `OPENAI_BASE_URL` / `OPENAI_MODEL` と、認証不要のローカルモデル向けのダミーキーは
image の非秘密設定として用意します。[Qwen の公式認証仕様](https://qwenlm.github.io/qwen-code-docs/en/users/configuration/auth/)
を参照してください。本 PR の自動 E2E は実モデルの精度・起動を検証するものではありません。

## ChatGPT への接続

stdio プロセスをブラウザの ChatGPT が直接起動する、という前提にはしていません。
2026-09-20 に確認した OpenAI 公式文書では、開発用 MCP の接続先は公開 HTTPS または
Secure MCP Tunnel で、Tunnel は stdio server へ転送できます。
Developer mode の利用可否はアカウントと workspace policy に依存します。
[接続・テストの公式手順](https://developers.openai.com/plugins/deploy/connect-chatgpt)

stdio のこの MVP は、利用可能なアカウントで公式 Tunnel を使う構成です。
Platform の Tunnel 設定から tunnel ID と runtime key を用意し、公式クライアントを導入します。
以下の command は適切に shell quote した絶対パスへ置き換えてください。

実際のキーをコマンド文字列や repository / shell history に保存しないでください。
以下は bash / zsh の端末で入力を非表示にして環境変数へ設定する例です。
（`set -x` 等のシェルトレースは無効にしてください。）

```sh
printf 'Tunnel runtime API key: '
read -rs CONTROL_PLANE_API_KEY
printf '\n'
export CONTROL_PLANE_API_KEY

tunnel-client init \
  --sample sample_mcp_stdio_local \
  --profile zaivern \
  --tunnel-id YOUR_TUNNEL_ID \
  --mcp-command 'zai mcp serve --workspace /absolute/project --image sha256:IMAGE_ID'
tunnel-client doctor --profile zaivern --explain
tunnel-client run --profile zaivern
```

Tunnel の runtime key は Tunnel client にだけ設定し、Agent image へ渡しません。
必要な workspace 関連付け・権限・認証方法は [Secure MCP Tunnel の公式文書](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)
に従います。ChatGPT の Developer mode で接続を追加し、通常の新規 Chat conversation の
tools menu から選択して試してください。公式文書には新規 conversation からの tool 選択手順がありますが、
すべてのプラン・通常 Chat モードでの提供を保証する記述ではありません。
**使用するアカウントの通常 Chat で実際に選択できるかは manual verification required です。**
Work でしか利用できないアカウントでは、今回の通常 Chat の要件を満たしたと扱いません。

HTTPS server / OAuth server はこの PR に追加していません。
独自の認証なし HTTP wrapper を公開する手順も提供しません。

## Tools と例

公開する tool は3つだけです。shell、ファイル単位の読み書き、任意 Git コマンドは公開しません。

| Tool | 引数 | 結果 |
| --- | --- | --- |
| `zaivern_run_task` | `instruction` | `task_id` |
| `zaivern_task_status` | `task_id` | state、summary、progress、changed_files、test/build status、diff_summary、error、duration |
| `zaivern_cancel_task` | `task_id` | cancellation_requested と現在の状態 |

対象は起動時に許可した workspace に固定されます。ChatGPT が絶対パスを知る必要はありません。
`workspace` を tool 引数に渡すと拒否されます。

run は worker を起動して直ちに返ります。status は terminal state になるまでポーリングします。
cancel の応答は停止完了ではありません。`cancelled` または他の terminal state を確認してください。
結果の取り込みが既に始まっていた場合は `completed` または `failed` になることがあります。既存編集の巻き戻しはしません。
Zaivern の検証が失敗した場合は `test_status=failed` と `state=failed` を返します。
修復を再試行しても検証に失敗した候補はホストへ取り込まず、`changed_files` は空にし、
`error` に取り込まなかった理由を返します。検証対象の候補は成功時だけ取り込みます。
対象外の検証は `not_verified` とし、それだけでは task を失敗にしません。
Agent 本文や検証出力が収集上限で切り詰められた場合は、秘密情報の文脈を失うため本文を省略します。
差分は既存の秘匿処理を通した変更範囲と前後3行で、200行を超える置換範囲は省略します。
差分応答全体が秘匿処理後に32 KiBを超える場合は、ローカル確認を求めるメッセージを返します。

例:

- “Inspect this repository and explain the architecture.”
- “Run the tests and explain the failures.”
- “Fix the failing tests and show me the diff.”
- 「このプロジェクトの失敗しているテストを直して、修正後にテストして diff を見せて。」

`Cargo.toml` が共有された場合、返却候補と開始時に固定した Cargo 検証入力を別の空のコンテナへ配置し、
`cargo test --offline` を実行します。Agent が追加した未共有ファイルは検証に使いません。
失敗時は最大2回、実際の出力を同じ Agent へ戻します。結果は出口コードで判定し、
Agent が「成功」と書いただけでは passed にしません。
`test_status=passed` はその検証 snapshot 内の Cargo テストの成功だけを表し、全動作の保証ではありません。
非 Rust ファイルの変更（manifest・データを含む）がある場合、Cargo が成功しても
`test_status=not_verified` とし、summary に実行した検証の範囲を明記します。
独立したビルド検証は実行しないため `build_status` は常に `not_verified` です。1回の検証上限は60秒です。
タイムアウト時は検証コンテナ全体を削除します。停止確認に失敗した場合は
`cancelled` とせず、container 識別名と cleanup error を返します。
他の build/test system は `not_verified` です。

### 編集コンテキストと Cargo 検証入力

Agent の編集用 snapshot は8 MiB / 1024ファイルまでです。リポジトリ全体の容量が
8 MiBを超えても、それだけで開始を拒否しません。指示中の既存相対パス
（例: `Fix src/chat_bridge/task.rs`）を優先し、Cargo.toml、entry point、Rust source、
その他の順に、パス順を固定して選びます。共有パス一覧を Agent に渡し、共有されなかったファイル数を Agent と結果に
明示します。選択は依存解析や全リポジトリ理解の保証ではありません。対象を絞った指示を
推奨します。Agent が未共有ファイルを追加・変更しても取り込みません。

Cargo 検証は別 snapshot を使います。開始時の元 manifest を既存 `toml` で解析し、
通常・dev・build・target 依存、`[patch]` / `[replace]` の `path`、workspace member と
使用される workspace 継承依存を再帰的に解決します。候補 manifest を理由にホストから
追加読取することはありません。`vendor/` 全体をコピーせず、到達した local package
だけを検証専用とし、Agent の ACP 許可集合・import 集合から除外します。
通常の workspace member は他の local dependency でなければ編集候補になります。

検証用には各 package の root の安全なテキストファイル、`src/tests/examples/benches/
assets/resources/i18n` と元 manifest の明示 target path を収集し、編集候補の bytes を
重ねます。上限は合計64 MiB / 8192ファイル / 128 packages、1ファイル1 MiBです。
64 MiBは既存256 MiB tmpfsの4分の1に制限し、残りをビルド等に残す予算です。
これは Agent に共有する容量の増量ではありません。上限を超えた Cargo 入力は拒否し、
危険・過大な Rust source を省いた状態でテスト成功とは判定しません。

依存 path は workspace 内の通常相対パスに限定し、canonical root・directory FD・
`O_NOFOLLOW`・hardlink検査を適用します。絶対パス、親 `..`（root内の兄弟参照も含む）、
秘密/hidden/config path、nested workspace は拒否します。member glob は末尾成分の
単一 `*`（例 `crates/*`）に対応し、他の glob は明示的に拒否します。
[Cargo の依存仕様](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html) と
[workspace 仕様](https://doc.rust-lang.org/cargo/reference/workspaces.html) の限定サブセットです。

非標準 build script の追加入力、除外対象の設定・秘密・binary、image にない registry
cache / toolchain が必要なら検証は失敗し、ホストへ取り込みません。Zaivern 自身でも
snapshot の初期化を通常CIで検査しますが、全テストの成功には適切な image と許可対象内の
ビルド入力が必要です。既存の config/秘密鍵ポリシーを都合よく解除しません。
検証専用ファイルは秘密領域ではありません。candidate のテストコードが読み、検証出力へ
書く可能性があるため、同じ秘密除外・出力redactionを適用します。ホスト上の元依存にも
外部編集がないことを import 前に照合します。

`instruction` は空白だけを除き1〜16384 Unicode code pointsです。別に JSONL の
MCPフレーム全体（改行・envelope・JSON escapeを含む）に64 KiB上限があります。
日本語10000文字は通常この範囲に収まりますが、emoji 16384文字は文字数内でも
フレーム上限を超えます。超過フレームは bounded read で接続を終了します。

## 実装と既存機構

```text
ChatGPT Chat → authenticated tunnel → MCP stdio → ChatBridge
  → TaskStore → TaskExecutor → local Docker snapshot
  → existing AcpClient / agents catalog / structured Phase
  → isolated edit → offline test → validated result import → status
```

main 調査時点は `d13185d` (0.24.5)。本体は単一 crate / `zai` binary で、
`tools/licgen` と `vendor/vt100` は別 manifest です。CLI は `main.rs` → `cli::try_run_cli`。
PTY Agent は `agents::AgentManager` / `session::Session`、ACP は `acp::AcpClient`、
監視は `supervisor`、チーム orchestration と保存は `team` にあります。
`git.rs` は外部 Git コマンド、`acp::FsHost` はクライアント FS、`config` は設定、
`agents::approvals` / `guard` / `lease` は承認と編集所有権を担います。
既存実行は std thread / channel と同期 I/O、主なエラーは `Result<_, String>` と
CloudError、テストはモジュール内 unit tests・実プロセステスト・nextest です。

既存の ACP はホスト上の任意プロセス起動を隔離しません。`FsHost` の root 検査も
Agent 自身の直接アクセスを制限しません。そのため今回の adapter は通常の Agent 起動へ
フォールバックせず、host bind mount を持たないコンテナを必須とします。
`AcpClient` の JSONL、session、キャンセル、構造化完了、権限応答を再利用し、
実行時限と出力収集には既存 Cloud Execution の `run_child` / `CollectSink` を使います。
タスクの内容をログへ記録せず、task ID / tool / state / duration / error 有無を stderr に出します。

MCP は `2024-11-05` の stdio / initialize / ping / tools の限定実装です。
[公式 rmcp](https://github.com/modelcontextprotocol/rust-sdk) は保守されている標準候補ですが、
Tokio / futures / schemars 等を要します。同期構成と3 toolsに限定した今回は既存 serde_json を使い、
新規依存・Cargo.lock の変更を避けました。独自 protocol 実装の保守責任が残ります。
HTTP を追加する際は SDK への置き換えを優先して再評価してください。
[chat-on-steroids](https://github.com/totec448-spec/chat-on-steroids) は high-level task lifecycle の参考で、
コードをコピーしていません。

## 安全モデルと制限

- canonical workspace を起動時に固定。`..` を拒否し、Unix の directory FD と
  `openat(O_NOFOLLOW)` で読み書きします。symlink、hardlink、特殊ファイルを拒否します。
- Agent へ渡すのは上限付きの text snapshot。隠しファイル/ディレクトリ、既知の資格情報・
  秘密鍵・設定ファイル名、target / node_modules / vendor を共有しません。
  **任意のソース中へ埋め込まれた秘密をすべて識別する DLP ではありません。共有元は所有者が確認してください。**
- Docker socket、host mount、host environment/credential、Git metadata は渡しません。
  外部通信なし、read-only root、capabilities 削除、no-new-privileges、PID 128 / CPU 2 / memory 4 GiB の上限を使います。
  workspace は容量256 MiBのタスク専用 tmpfs volume で、stdinから配置し、
  コンテナ全体をpauseして結果を取得します。volumeはコンテナ終了後に削除します。
  Docker daemon、OS、指定した image は信頼する基盤です。
- Agent の permission request は Qwen の `_meta.toolName`、kind、引数、locations を照合します。
  `read_file` / `edit` / `grep_search` の既存共有ファイル完全一致だけを一度限り許可します。
  metadata 欠落、未知引数、別 session、ディレクトリ単位の探索・glob、永続許可は拒否します。
  [Qwen の ACP 契約](https://github.com/QwenLM/qwen-code/blob/main/packages/cli/src/acp-integration/session/Session.ts)
  に対応しない image は fail closed となります。
  ACP metadata は peer の申告で、Agent は承認要求を省略することもあります。
  このフィルタは承認応答の制限です。ホストの強制的な隔離境界は Docker と snapshot/import が担います。
  execute / delete / move / fetch / unknown は拒否し、GUI の自動承認 policy は継承しません。
  `git push`、commit、merge、rebase、reset、clean、`rm -rf`、sudo をホストで実行する経路はありません。
  ホストの status / log / branch / show もこの限定 MVP では取得しません。
- 戻すのは共有済みの既存 text file だけです。追加・削除・rename・binary・権限変更は未対応。
  出力 archive はホストへ展開せず、既知の path ごとに regular-file payload だけを読みます。
  取り込み前に元内容との一致と既存の write lease を確認します。
  複数ファイルの取り込みは transaction ではなく、I/O 失敗時は部分適用の可能性と対象を返します。
  作業中に別の editor / process から同じ workspace を変更しないでください。
- 1 task / server、記録128件、1 file 1 MiB、Agent snapshot 8 MiB / 1024 files、task 30分。
  編集対象を上限内に選択し、検証入力の上限超過は拒否します。task store はメモリ上のみで、再起動すると task ID は失効します。
  diff response は32 KiBまでで、超過時は省略した事実を返します。
- macOS / Linux のローカル実行だけを提供します。Windows、外部モデルへの安全な gateway、
  interactive approval、汎用 build/test runner、永続 task store は未対応です。
  実モデルを使った品質・認証・メモリ容量の検証は別途必要です。
  CPU / memory / snapshot の固定上限は初期版のホスト占有量を制限するための値で、
  大規模リポジトリやモデルに対する性能保証ではありません。容量超過や Agent の OOM は失敗として返します。

## LLM 不要の検証

```sh
docker build --network=none -f tools/mcp-fixture.Dockerfile -t zaivern-mcp-fixture tools
cargo build --bin zai
ZAIVERN_MCP_TEST_IMAGE="$(docker image inspect zaivern-mcp-fixture --format '{{.Id}}')" \
  cargo test real_stdio_container_agent_edit_test_diff_and_cancel -- --ignored --nocapture
```

fixture は実際の ACP process として起動し、危険操作の拒否、host FS capability の拒否、
外部ネットワーク不可、編集、実際の offline Rust test、diff、実行中 cancel を検証します。
未共有の追加ファイルに依存する候補を、検証成功と判定しない反証も含みます。
通常CIの snapshot tests は Docker / LLM / API キーを必要とせず、大規模ソース選択、
local dependency / patch / workspace、候補manifest改変、逸脱・リンク・外部変更を検査します。
固定fixtureを使った `cargo test --offline` も実行します（ユーザーコードのホスト実行ではありません）。
Docker E2Eは専用 image を明示して起動する opt-in のままです。検証用 local dependency を
実コンテナへ配置する経路も、この E2E で確認します。

## Future Cloud Runner

protocol と `TaskExecutor` / `TaskStore` は分離しています。将来は既存の
Cloud Execution target / transport を使う executor や永続 store に差し替えられます。
この PR では Cloud Runner、multi-tenant、課金、別 UI、HTTP/OAuth server は実装しません。
