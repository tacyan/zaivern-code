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
//! ## 統合担当への申し送り
//!
//! CLI (`zai negotiate offer|allocate|deal`) の入口は [`cli_main`] として
//! 公開してある。`src/cli.rs` は共有ファイルなので**こちらでは配線していない** —
//! サブコマンドの分岐へ次の 1 行を足すと繋がる:
//!
//! ```ignore
//! "negotiate" => return Some(crate::features::negotiate::cli_main(&args[1..])),
//! ```
//!
//! **罠**: `zai` は知らない語をワークスペース指定として扱い GUI を起動する。
//! 上の 1 行を入れる前に `zai negotiate ...` を叩くと**窓が生える**ので、
//! `cli::is_cli_subcommand("negotiate")` の門も同じ commit で入れること。
//!
//! 打鍵は 1 つも取っていない。欲しくなったら `BindAction` を増やさずに
//! `Cmd::Feature("negotiate.panel")` を直に指すのが安い。

#[path = "../negotiate.rs"]
mod imp;

pub use imp::FEATURE;

/// `zai negotiate <sub>` の入口。`src/cli.rs` の dispatch から呼ばれる。
///
/// **いまはまだ呼び手がいない。** `src/cli.rs` は 8 本のブランチが同時に
/// 触っている共有ファイルなので、こちらでは配線しない (上の申し送りの 1 行が
/// 入った瞬間に呼び手が付く)。`allow` を置いているのはその 1 行までの間だけで、
/// **このモジュールに他の `allow` は無い** — `never used` は
/// 「作ったのに繋いでいない」の検出器なので、潰してよいのはここだけである。
/// 入口が生きていることは `negotiate::tests::cliの入口と終了コード` が確かめる。
#[allow(unused_imports)]
pub use imp::cli_main;
