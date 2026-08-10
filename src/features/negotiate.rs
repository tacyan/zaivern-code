//! 🤝 行域の交渉 (ぶつかった要求を近くの空き域へ振り替える) の**登録だけ**。
//! 実体は `src/negotiate.rs`。
//!
//! このファイルを置くだけで機能が繋がる。`main.rs` の `mod` 一覧にも
//! `feature.rs` のレジストリにも触らない (build.rs が `src/features/*.rs` を
//! 走査して拾う)。
//!
//! ## なぜ `#[path]` で実体を引き込むのか
//!
//! 実体は CLAUDE.md の指示どおり `src/negotiate.rs` に置いている。ただし
//! `src/<名前>.rs` をクレートへ入れるには通常 `main.rs` の `mod` 一覧へ
//! 1 行足す必要があり、**それはまさに並列ブランチが取り合う共有行**である。
//! `#[path]` を使うと、`main.rs` を 1 バイトも触らずに実体を
//! `crate::features::negotiate::imp` として取り込める (`train` と同じ形)。
//!
//! 実体が要求する共有面は**ゼロ**: `app.rs` / `palette.rs` / `feature.rs` /
//! `config.rs` / `keybinds.rs` / `main.rs` / `build.rs` / `cli.rs` の
//! どれも触っていない。設定 `negotiate.max_shift` は `feature::Setting` の
//! 宣言で持つので `config.rs` への追記が要らず、打鍵は 1 つも取っていない。
//!
//! ## 姉妹機能との住み分け
//!
//! * `lease.rs` — 同じファイルを 2 人に触らせない (**起こさない**側)
//! * `region.rs` — 同じファイルでも違う行なら通す (**通す**側の土台)
//! * `conflict.rs` — 近い行の衝突を先に見せる (**見せる**側)
//! * `train.rs` — 見つけた後に順番を決めて統合する側
//! * `negotiate.rs` — **断るしかなかった要求を、断らずに振り替える側**。
//!   実測で「衝突 0・人手 0 なのに 64 件中 55 件を断っていた」ところ。
//!
//! ## 到達経路 (配線済み)
//!
//! CLI (`zai negotiate offer|allocate|deal|serve|ask|help`) の入口は
//! [`cli_main`]。`src/cli.rs` の dispatch と `is_cli_subcommand("negotiate")`
//! の門は**統合担当が直列で入れた**ので、もう申し送りは残っていない。
//!
//! **罠 (入れる前に踏んだもの)**: `zai` は知らない語をワークスペース指定として
//! 扱い GUI を起動する。dispatch だけ入れて `is_cli_subcommand` の門を忘れると
//! `zai negotiate ...` で**窓が生える**ので、2 つは必ず同じ commit で入れる。
//!
//! メッシュの上で実際に回す `serve` / `ask` は [`crate::negomesh`] が持つ
//! ([`cli_main`] からそちらへ委譲する)。`negotiate` は純関数のまま、
//! `mesh` は行域を知らないまま保つための分割で、この 2 つを繋ぐ層が橋だけになる。
//!
//! 打鍵は 1 つも取っていない。欲しくなったら `BindAction` を増やさずに
//! `Cmd::Feature("negotiate.panel")` を直に指すのが安い。

#[path = "../negotiate.rs"]
mod imp;

pub use imp::FEATURE;

/// `zai negotiate <sub>` の入口。`src/cli.rs` の dispatch から呼ばれる。
///
/// **このモジュールに `allow` は 1 つも無い。** `never used` は
/// 「作ったのに繋いでいない」の検出器なので、潰さずに配線で消す。
/// 入口が生きていることは `negotiate::tests::cliの入口と終了コード` が
/// (サブコマンド一覧と終了コードの説明ごと) 確かめる。
pub use imp::cli_main;

/// 交渉の公開面。**橋 (`crate::negomesh`) がここから引く。**
pub use imp::{allocate, decode, encode, offer, Deal, Offer, Want};
