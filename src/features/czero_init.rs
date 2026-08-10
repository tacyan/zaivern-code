//! 🚦 競合ゼロの導入・診断・実証・撤去 (`zai czero …`) の**登録だけ**。
//! 実体は `src/czero_init.rs`。
//!
//! このファイルを置くだけで機能が繋がる。`main.rs` の `mod` 一覧にも
//! `feature.rs` のレジストリにも触らない (`build.rs` が `src/features/*.rs` を
//! 走査して拾う)。
//!
//! ## なぜ `#[path]` で実体を引き込むのか
//!
//! 実体は CLAUDE.md の指示どおり `src/czero_init.rs` に置いている。ただし
//! `src/<名前>.rs` をクレートへ入れるには通常 `main.rs` の `mod` 一覧へ
//! 1 行足す必要があり、**それはまさに並列ブランチが取り合う共有行**である。
//! `#[path]` を使うと、`main.rs` を 1 バイトも触らずに実体を
//! `crate::features::czero_init::imp` として取り込める (`train` / `czero` と同じ形)。
//!
//! 実体が要求する共有面は**ゼロ**: `app.rs` / `palette.rs` / `feature.rs` /
//! `config.rs` / `keybinds.rs` / `main.rs` / `build.rs` / `cli.rs` の
//! どれも触っていない。
//!
//! ## 姉妹機能との住み分け
//!
//! * `guard.rs` — git を関所にするフック**だけ**を設置する
//! * `union.rs` — merge driver **だけ**を登録する
//! * `lease.rs` — 行域の台帳**だけ**を持つ
//! * `czero.rs` — いまどこまで守られているかを**見る**パネル
//! * `czero_init.rs` — **上の全部を 1 コマンドで入れ、診断し、実際に試し、
//!   綺麗に戻す**側。ここが無いと「どのリポジトリでも競合が起きない」は
//!   手順書の話で終わる。
//!
//! ## 統合担当への申し送り (この 2 行で繋がる)
//!
//! CLI (`zai czero init|doctor|verify|uninstall`) の入口は [`cli_main`] として
//! 公開してある。`src/cli.rs` は共有ファイルなので**こちらでは配線していない** —
//! 次の 2 行を足すと繋がる:
//!
//! ```ignore
//! // (1) is_cli_subcommand() の門へ (**これを忘れると窓が生える**):
//!             | "czero"
//! // (2) try_run_cli() の match へ:
//!         "czero" => crate::features::czero_init::cli_main(rest),
//! ```
//!
//! (1) が要るのは、`zai` が**知らない語をワークスペース指定として扱って
//! GUI を起動する**ため。`zai czero doctor` が「czero という名前のフォルダを
//! 開く」に化ける (`coedit` で実際に踏まれた罠)。
//! 打鍵の要求は無い (`keybinds.rs` は 1 バイトも要らない)。

#[path = "../czero_init.rs"]
mod imp;

pub use imp::FEATURE;

/// `zai czero <sub>` の入口。`src/cli.rs` の dispatch から呼ばれる。
///
/// **まだ配線されていない** (共有ファイルなので統合担当が直列で入れる)。
/// それまでこの関数はどこからも呼ばれないが、GUI のパレット項目
/// (`czero_init.run`) が同じ処理へ到達するので、機能そのものは死んでいない
/// (`dead_code` の抑止は実体側 `src/czero_init.rs` の `cli_main` に付けてある)。
pub use imp::cli_main;

// **再エクスポートを実際に使う。** これが無いと配線されるまで
// `unused import` が出続け、本物の「作ったのに繋いでいない」警告が
// その中に埋もれる (CLAUDE.md の検出器を鈍らせない)。
// ついでに、統合担当が `cli.rs` へ足す 1 行の**署名を型で固定**する:
//   "czero" => crate::features::czero_init::cli_main(rest),
const _: fn(&[String]) -> i32 = cli_main;
