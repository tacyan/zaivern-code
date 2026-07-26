# CLAUDE.md — Zaivern Code 開発ガイド（AIエージェント向け）

このファイルはリポジトリで作業する全ての AI エージェント／開発者が最初に読むこと。

## 絶対ルール

- **ハードコーディング禁止。** どの環境（macOS / Windows / Linux、任意のユーザー名・ホーム・ロケール）でも動くコードのみ。
  - パスは `std::env::temp_dir()` / `dirs` クレート / 設定から導出。`/tmp`・`/Users/...`・`C:\` の直書き禁止。
  - OS 差分は `cfg!(windows)` 等で分岐し、**両側を実装**する。
  - エージェント固有値（resume フラグ、コマンド名等）は `agents.rs` のカタログにデータとして持つ。
- **egui 0.29 固定**（アップグレード禁止）・rustc 1.88+。
- **vendor/vt100 はパッチ済みベンダリング。** `visible_rows` 修正を外すと deep scrollback で debug パニックが再発する。バージョンアップ時は必ずパッチを移植。
- main へ直接コミットしない。作業はブランチ＋隔離ワークツリー（`.claude/worktrees/`）で行う（複数の AI エージェントが同時編集する前提）。

## テストと検証

- 検証は**ローカル優先**。CI は最小構成 + `Swatinem/rust-cache` 必須。
- `terminal::` は実 PTY テスト。1 つの `cargo test` プロセス内で走らせると子プロセスツリーが蓄積し **Linux ランナーを殺す**ため、CI は cargo-nextest（テスト毎プロセス + `pty` test-group で直列化 + slow-timeout のプロセスグループ kill）で実行する。設定は `.config/nextest.toml`。
- ローカル実行例: `cargo nextest run --profile ci`（全量）/ `cargo test session::`（モジュール別）。
- テストは `crate::test_util::unique_temp_dir` を使い、実 `~/.zaivern` に触れない。
- GUI の動作検証はプロセス生存確認で行う（ヘッドレスでは UI を目視できないため）。
- **終了済みセッションへ kill を撃たない**（PID 再利用の巻き添え防止。既存ガードを regress させない）。

## 既知の罠

- egui-winit 0.29 はペーストコード（⌘V / Ctrl+V）の **press イベントを飲み込む** — クリップボード処理は release イベント側で検知する。
- 全画面切り替えのレースは「前フレーム比の矩形安定」で判定する（サブディスプレイでのズレ対策）。
- Windows テスト: `powershell -File` 化・cmd ビルトインのスリーパー・パス区切り/コードページ・URL のドライブコロン誤検知に注意。
- macOS 26 で `❯` グリフが消えるフォント問題あり。
- クラッシュは `panic.log` とフレームガードで捕捉する仕組みがある。
- PTY テストのシェルスクリプトに長い `sleep` を書かない（プロセス残留の温床）。

## アーキテクチャのメモ

- スマホリモート機能は `remote.rs`（ポート 8899〜）。
- supervisor の診断（Diagnostician）は in-process で完結させる。外部 CLI エージェントへ投げない。
- セッション永続化は `~/.zaivern/sessions/`、PTY 生ログは `~/.zaivern/term_logs/<workspace_hash>/`（フォルダ別）。
