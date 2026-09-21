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
2026-09-22 に確認した OpenAI 公式文書では、開発用 MCP の接続先は公開 HTTPS または
Secure MCP Tunnel で、Tunnel は stdio server へ転送できます。
Developer mode の利用可否はアカウントと workspace policy に依存します。
[接続・テストの公式手順](https://developers.openai.com/plugins/deploy/connect-chatgpt)

stdio のこの MVP は、利用可能なアカウントで公式 Tunnel を使う構成です。
Platform の Tunnel 設定から tunnel ID と runtime key を用意し、公式クライアントを導入します。
この手順の確認対象は **tunnel-client v0.0.14** です。
配布物 `tunnel-client-v0.0.14-<OS>-<ARCH>` に含まれる `tunnel-client` を使用します。
`tunnel-client-runtime-cloudflared` 単体は、この `init` / `doctor` / `run` 手順の起点ではありません。
端末で `tunnel-client --version` が `0.0.14` を示すことを確認してください。
ローカルの darwin-amd64 配布物では
`0.0.14+0f870e50a973fa820d4c409000059e181e8d242b` と各サブコマンドの help を確認しました。
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
  --mcp-command '/absolute/path/to/zai mcp serve --workspace /absolute/project --image sha256:IMAGE_ID'
tunnel-client doctor --profile zaivern --explain
tunnel-client run --profile zaivern --mcp.stdio-send-initialized-notification
```

`--mcp-command` の `zai` は修正版をビルドした実行ファイルの絶対パスにしてください。
PATH 上の古い v0.24.7 バイナリを指定したままでは、ソースを修正しても discovery は変わりません。
同じ profile が存在する場合は無条件に `init --force` で上書きせず、既存設定の MCP command を更新します。

v0.0.14 の initialized 自動通知は **opt-in** です。上記 run オプションにより、
旧 MCP の `initialize` 成功後に `notifications/initialized` を stdio server へ送信します。
クライアントが後から同じ通知を送った場合は tunnel-client 側が重複を抑制します。
[v0.0.14 の実装](https://github.com/openai/tunnel-client/blob/v0.0.14/pkg/mcpclient/serialized_forwarding_transport.go)
この通知を省略した旧 MCP の tool 呼び出しを、Zaivern が暗黙に初期化済みとして許可することはありません。

Tunnel の runtime key は Tunnel client にだけ設定し、Agent image へ渡しません。
必要な workspace 関連付け・権限・認証方法は [Secure MCP Tunnel の公式文書](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)
に従います。ChatGPT の Developer mode で接続を追加し、通常の新規 Chat conversation の
tools menu から選択して試してください。公式文書には新規 conversation からの tool 選択手順がありますが、
すべてのプラン・通常 Chat モードでの提供を保証する記述ではありません。
**使用するアカウントの通常 Chat で実際に選択できるかは manual verification required です。**
Work でしか利用できないアカウントでは、今回の通常 Chat の要件を満たしたと扱いません。

Connector / Plugin 作成では Connection を **Tunnel**、対象 Tunnel を **Zaivern**、
Authentication を **None** とします（この stdio server 自体は OAuth を公開していません）。
`run` は Connector 登録・利用中も起動したままにします。
Tunnel UI の Health=live / Ready=ready / connected は転送経路の状態です。
`doctor` 成功、`tunnel_service_status="200"`、`/response` の HTTP 200 は
MCP の `result` 成功を保証しません。`error.code` / `error.message` も確認してください。

HTTPS server / OAuth server はこの PR に追加していません。
独自の認証なし HTTP wrapper を公開する手順も提供しません。

### Discovery と protocol version

`src/features/chat_bridge.rs` → `src/chat_bridge/mod.rs` → `protocol::serve` が
`zai mcp serve` の stdio 経路です。応答は改行区切りの JSON-RPC のみを stdout に出し、
ログは stderr に出します。

- 旧ハンドシェイク: `initialize` → `notifications/initialized` → `server/discover` → `tools/list`。
  `2024-11-05` / `2025-06-18` / `2025-11-25` の要求には同じ版を返します。
  未対応版には対応する旧版 `2025-11-25` を提示し、利用可否はクライアントが判断します。
  batching が必要な `2025-03-26` は対応版として掲示しません。
- 新仕様 `2026-07-28`: request の `params._meta` に
  `io.modelcontextprotocol/protocolVersion` と `io.modelcontextprotocol/clientCapabilities` を付けます。
  initialize は不要で、`server/discover` または `tools/list` / `tools/call` から開始できます。
  対応しない要求版には `-32022` と `error.data.supported` / `requested` を返します。
- discovery は `resultType: "complete"`、`supportedVersions`、`capabilities: {"tools":{}}`、
  `_meta["io.modelcontextprotocol/serverInfo"]` を返します。草案 SEP の top-level `serverInfo` とは異なります。
- discovery と最新仕様の tools/list は、必須の `cacheScope: "private"`、`ttlMs: 0` を返し、共有キャッシュや古いツール定義の再利用を要求しません。
  tools の一覧は `tools/list` で取得します。resources / prompts は掲示せず、対応しないメソッドには
  `-32601` を返します。存在しないツールは `-32602`、実行・引数の失敗は tool result の `isError` です。

根拠: [MCP discovery](https://modelcontextprotocol.io/specification/2026-07-28/server/discover)、
[新旧バージョンの互換性](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning)、
[旧 initialize の版交渉](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle)。

修正前の v0.24.7 の `protocol.rs` には `server/discover` の分岐がなく、通知後でも
`-32601 Method not found`、通知前なら `-32000 Initialize the connection first` を返していました。
また版の応答は `2024-11-05` 固定でした。固定値だけを登録失敗の原因とは断定できませんが、
新仕様の discovery を要求するクライアントには必要な応答がありませんでした。
v0.0.14 の [dispatcher](https://github.com/openai/tunnel-client/blob/v0.0.14/pkg/dispatcher/internal/processor.go)
は JSON-RPC の error も正常に転送するため、HTTP 200 はこの不整合を否定しません。
実アカウントの request/response 本文と Connector 再登録の成功は、下記 manual acceptance で確認します。

## Manual acceptance checklist

**Manual acceptance: NOT YET VERIFIED** — 自動CIはstdio → Docker fixture ACPまでです。
実ChatGPTアカウント・Secure MCP Tunnel・実Qwen/モデルでの成功は未確認です。
隔離した試験用workspaceとバックアップを用意し、次を実アカウントで記録してください。

1. 上記手順でSecure MCP Tunnelを起動し、doctorの結果を確認する。
2. 利用可能なChatGPT Developer modeでconnectorを追加する。
3. 新しい**通常Chat conversation**を開き、connectorを選択する。
4. `zaivern_run_task`、`zaivern_task_status`、`zaivern_cancel_task`の3つが表示されることを確認する。
5. 「ファイルを変更せず構成を説明して」と依頼する。これは指示でありread-only capabilityではないため、前後のhost bytesも比較する。
6. runの`task_id`を記録し、statusをpollして`completed`を確認する。
7. 共有される既存ファイル1つだけの小さな修正を依頼する。
8. terminal statusの`changed_files`がその1ファイルであること、`diff_summary`とhost実ファイルの変更が一致することを確認する。
9. Cargo workspaceでは`test_status`を確認する。下記のcoverage条件外は`not_verified`、Cargo失敗は`failed`でimportゼロが正しい。
10. 長時間taskを開始してcancelし、statusをpollしてterminal stateを確認する。
11. import開始前にcancelが受理された場合、host bytesが不変であることを確認する。開始後は巻き戻されない。
12. tool argumentsへ`workspace`を追加すると拒否され、別workspaceへアクセスできないことを確認する。
13. unknown tool、無効な`task_id`、不正引数が適切にエラーになることを確認する。ChatGPTが不正呼出しを生成できない場合は同じ接続のprotocol clientで確認し、実施手段を記録する。

### Manual acceptance record

- Date:
- Zaivern commit:
- ChatGPT plan/workspace:
- 通常Chatで利用可能か（Workのみなら不合格）:
- Tunnel client version:
- Qwen Code version / image ID:
- Docker version:
- OS:
- Result:
  - tools visible:
  - run_task / task_id:
  - status / terminal state:
  - edit/import / host bytes:
  - diff / test_status:
  - cancel / host bytes:
  - invalid arguments / unknown tool / task_id:
- 未実施項目・失敗内容・再現手順:

## Tools と例

公開する tool は3つだけです。shell、ファイル単位の読み書き、任意 Git コマンドは公開しません。
annotations は全toolで `openWorldHint=false`。run は既存ファイルの上書きを伴い得るため
`readOnlyHint=false / destructiveHint=true`、status は `true / false`、cancel は `false / false` です。
これらはクライアント向けのヒントであり、実行時の隔離・検証・権限検査を置き換えません。

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

`Cargo.toml` が共有され、必要なCargo入力が**すべてAgentにも共有されている場合だけ**、
共有候補を別の空のread-onlyコンテナへ配置し、`cargo test --workspace --frozen`を実行します。
実際の検証失敗は最大2回修復を依頼し、最終失敗ではimportしません。出力はredactionを通します。
Agentが追加した未共有ファイルは検証に使わず、Agentの成功宣言だけではpassedにしません。

`verification_only`が1つでもあれば、**Cargo metadata / build / testを一切起動しません**。
判定は明示的な`NotVerified`で、`passed=true`の代用ではありません。repairも発生せず、
`test_status=not_verified`、`build_status=not_verified`と「未共有入力が必要なため検証未実行」の
summaryを返します。候補コードと未共有入力を同じ実行環境へ置かないため、stdoutだけでなく
pass/fail、repair回数、import有無、候補コードのsleepによる時間oracleも防ぎます。
通常のroot identity・共有ファイル競合・write lease・cancel/import gate・cleanup検査を
通過した既存共有ファイルの変更は取り込み可能です。これは正しさを検証済みという意味ではありません。
未共有ファイルはimport時にも再読込・比較せず、その変更をimport可否の判定に使いません。
未共有入力があるtaskでは、Cargo.tomlの変更はimportを拒否します。依存を削除して次のtaskで
秘匿対象を共有対象へ変える経路を防ぐためです。共有範囲の変更は所有者がホスト側で行ってください。

以下はshared-only検証を実行する場合の仕様です。
`--workspace` は root package / default-members に限定せず全 workspace member を対象にします。
`--frozen` により lockfile の生成・更新とネットワーク取得を禁止します。有効な `Cargo.lock` を
開始前に用意してください。欠落・不整合は検証失敗となり、候補をホストへ取り込みません。

`test_status=passed` は、Cargo テスト成功に加え、変更した全ファイルが実コンパイルを確認できた
Rust crate root である場合だけです。verifier 内の `cargo metadata --format-version=1 --frozen` で
全依存に build script / proc-macro がないことを確認し、成功した
`cargo test --workspace --frozen --no-run --message-format=json` の compiler-artifact と照合します。
この証跡はテスト実行前に固定し、host のファイル探索・共有許可には使いません。
任意のコンパイル時コードがある場合、出力を偽造できるため証跡を信用しません。
未使用 `.rs`、通常 module、`include_str!` のデータ、非 Rust、証跡の欠落・上限超過・未知形式は
`not_verified` です。dep-info のファイル一覧だけで Rust の検証成功とは判定しません。
Cargo 自体の失敗は `failed` のままで、`not_verified` に置き換えません。
独立したビルド検証は実行しないため `build_status` は常に `not_verified` です。
metadata・no-run・test の各コマンドは最大60秒、task 全体は30分です。
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
だけを未共有入力として識別し、Agent の ACP 許可集合・import 集合から除外します。
通常の workspace member は他の local dependency でなければ編集候補になります。

必要入力の識別には各 package の root の安全なテキストファイル、`src/tests/examples/benches/
assets/resources/i18n` と元 manifest の明示 target path を収集します。上限は合計64 MiB / 8192ファイル / 128 packages、1ファイル1 MiBです。
ホスト内の識別用メモリにも既存の64 MiB上限を維持します。実行対象の共有入力は8 MiB以内です。
これはAgentやverifierへ未共有内容を渡す予算ではなく、ホスト内の入力識別の上限です。上限を超えた Cargo 入力は拒否し、
危険・過大な Rust source を省いた状態でテスト成功とは判定しません。

依存 path は workspace 内の通常相対パスに限定し、canonical root・directory FD・
`O_NOFOLLOW`・hardlink検査を適用します。絶対パス、親 `..`（root内の兄弟参照も含む）、
秘密/hidden/config path、nested workspace は拒否します。member glob は末尾成分の
単一 `*`（例 `crates/*`）に対応し、他の glob は明示的に拒否します。
[Cargo の依存仕様](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html) と
[workspace 仕様](https://doc.rust-lang.org/cargo/reference/workspaces.html) の限定サブセットです。

shared-onlyの実行で、非標準 build script の追加入力、除外対象の設定・秘密・binary、image にない registry
cache / toolchain が必要なら検証は失敗し、ホストへ取り込みません。Zaivern 自身でも
snapshotの初期化を通常CIで検査しますが、未共有入力があるため自動Cargo検証は行いません。既存の config/秘密鍵ポリシーを都合よく解除しません。
未共有入力にも同じ秘密除外ポリシーを適用します。元manifestによる入力識別・容量・構造の
検査はホスト内で継続しますが、未共有入力を含む実行treeのstagingは拒否します。
stdout/stderrの秘匿だけでは、候補コードが作る成功/失敗・時間のoracleを防げないためです。
shared-only verifierの出力には従来どおりredactionを適用します。

`instruction` は空白だけを除き1〜16384 Unicode code pointsです。別に JSONL の
MCPフレーム全体（改行・envelope・JSON escapeを含む）に256 KiB上限があります。
ASCII・日本語・emoji の16384文字を受け付け、最悪の surrogate-pair escapeでも
192 KiB＋envelopeの余裕があります。16385文字はtool validation errorとなり接続は維持します。
巨大な追加metadata等も含めてframe上限を超えた場合は、上限＋1 byteまでの bounded readで接続を終了します。

## 実装と既存機構

```text
ChatGPT Chat → authenticated tunnel → MCP stdio → ChatBridge
  → TaskStore → TaskExecutor → local Docker snapshot
  → existing AcpClient / agents catalog / structured Phase
  → isolated edit → shared-only verification / not_verified → checked import → status
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
  コンテナ全体をpauseして結果を取得します。verifier は準備コンテナ削除後、入力 volume を
  read-only で mount します。Cargo.lock / manifest / 共有ソースを変更・削除・追加できません。未共有入力は配置しません。
  ビルド生成物は別の空の `/target` tmpfs（256 MiB）へ置きます。volume は最後のコンテナ終了後に削除し、
  Agent・準備コンテナ・verifier・volume の cleanup 失敗はいずれも import を禁止します。
  Docker daemon、OS、指定した image は信頼する基盤です。
- import前に、保持中のroot FDと現在のworkspace pathの `dev + ino` を照合します。
  pathは全成分を `O_NOFOLLOW` で開き直し、rename/recreate・symlink差し替え・消失を
  lease判定前および全ファイル検証後の書込み開始前に拒否します。旧directoryにも書き込みません。
  import全体は既存のcancel/import gate内で実行します。外部プロセスのrenameと書込みを
  原子的にロックする仕組みではないため、import中の外部編集・移動は避けてください。
- Agent の permission request は Qwen の `_meta.toolName`、kind、引数、locations を照合します。
  `read_file` / `edit` の既存共有ファイル完全一致だけを一度限り許可します。
  metadata 欠落、未知引数、別 session、ディレクトリ単位の探索・glob、永続許可は拒否します。
  [Qwen ACP Session（監査commit 878a32f）](https://github.com/QwenLM/qwen-code/blob/878a32f86f8e2a4167f63c84ee41d33a4a8090b3/packages/cli/src/acp-integration/session/Session.ts)
  のpermission requestは`_meta.toolName`、mapped kind、`rawInput=args`、`invocation.toolLocations()`を送ります。
  [read_file](https://github.com/QwenLM/qwen-code/blob/878a32f86f8e2a4167f63c84ee41d33a4a8090b3/packages/core/src/tools/read-file.ts)
  は`file_path`と任意の`offset/limit/pages`、
  [edit](https://github.com/QwenLM/qwen-code/blob/878a32f86f8e2a4167f63c84ee41d33a4a8090b3/packages/core/src/tools/edit.ts)
  は`file_path/old_string/new_string`と任意の`replace_all`だけを対応範囲とします。
  `old_string`は空を拒否し、新規ファイル作成を許可しません。UI用の`modified_by_user/ai_proposed_content`も未対応です。
  [grep_search](https://github.com/QwenLM/qwen-code/blob/878a32f86f8e2a4167f63c84ee41d33a4a8090b3/packages/core/src/tools/grep.ts)
  の正式引数は`pattern`と任意の`path/glob/limit`ですが、pathを検索用working directoryにする実装です。
  単一共有ファイルだけを検索する契約ではないため、bridgeでは**grep_searchを明示拒否**します。
  path省略、directory、globだけでなく共有ファイル完全一致の指定も拒否します。
  Agentは共有パス一覧から必要なファイルを`read_file`で読んでください。
  read/editのlocationsは指定時に対象ファイルと一致させます。未知引数は拒否し、
  実Qwen/モデルとの運用確認は上記manual acceptanceで別途行います。
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

base imageの取得にはネットワークが必要です。CIと同じく取得を先に行い、
その後のbuildのRUN命令はネットワークなしで実行します。`--network=none`はbuild全体の
registry通信を遮断する指定ではありません。baseはRust公式の保守tagを使用し、実行時は
ビルド済みfixtureのimmutable image IDを固定します（bit-for-bit再現ビルドの保証ではありません）。

```bash
docker pull rust:1-bookworm
docker build --network=none -f tools/mcp-fixture.Dockerfile -t zaivern-mcp-fixture:test tools
cargo build --locked --bin zai
ZAIVERN_MCP_TEST_IMAGE="$(docker image inspect zaivern-mcp-fixture:test --format '{{.Id}}')"
[[ "$ZAIVERN_MCP_TEST_IMAGE" =~ ^sha256:[0-9a-f]{64}$ ]]
export ZAIVERN_MCP_TEST_IMAGE
cargo test --locked --bin zai \
  features::chat_bridge::imp::e2e_tests::real_stdio_container_agent_edit_test_diff_and_cancel \
  -- --exact --ignored --nocapture
docker ps -a --filter name=zaivern-mcp-
docker volume ls --filter name=zaivern-mcp-
```

`running 1 test`と成功を確認してください。開始前にも資源一覧を保存し、今回生成した
container/volumeの残存がないことを比較します。他のtaskの資源は削除しません。

fixture は実際の ACP process として起動し、危険操作の拒否、host FS capability の拒否、
外部ネットワーク不可、編集、実際の offline Rust test、diff、実行中 cancel を検証します。
未共有の追加ファイルに依存する候補を、検証成功と判定しない反証も含みます。
通常CIの snapshot tests は Docker / LLM / API キーを必要とせず、大規模ソース選択、
local dependency / patch / workspace、候補manifest改変、逸脱・リンク・外部変更を検査します。
固定fixtureを使った `cargo test --workspace --frozen` も実行します（ユーザーコードのホスト実行ではありません）。
Docker E2E は Ubuntu の通常 CI で専用 image をビルドし、immutable image ID を指定して明示実行します。
他 OS の通常テストでは Docker を要求しません。未共有入力の読み出し・hex出力・bitに応じた
panic/sleepを試みる攻撃fixtureは、検証を起動せずnot_verifiedで同じ安全import経路に進み、
repairを発生させないことを検査します。通常CIでもDockerコマンド呼出しゼロとstagingゼロを
故障注入で確認します。shared-onlyの検証失敗時import拒否とrepair成功も検査します。
non-virtual workspace / default-members、未使用 Rust、lockfile 欠落・不整合、入力 volume への
書き込み拒否も通常 Cargo 回帰と Docker E2E で検査します。
CI はテスト前後の container / volume を比較し、新たな残存を失敗として扱い、回収を試みます。

## Future Cloud Runner

protocol と `TaskExecutor` / `TaskStore` は分離しています。将来は既存の
Cloud Execution target / transport を使う executor や永続 store に差し替えられます。
この PR では Cloud Runner、multi-tenant、課金、別 UI、HTTP/OAuth server は実装しません。
