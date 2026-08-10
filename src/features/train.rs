//! 🚃 マージトレイン (並列エージェントの成果を順次統合する) の**登録だけ**。
//! 実体は `src/train.rs`。
//!
//! このファイルを置くだけで機能が繋がる。`main.rs` の `mod` 一覧にも
//! `feature.rs` のレジストリにも触らない (build.rs が `src/features/*.rs` を
//! 走査して拾う)。
//!
//! ## なぜ `#[path]` で実体を引き込むのか
//!
//! 実体は CLAUDE.md の指示どおり `src/train.rs` に置いている。ただし
//! `src/<名前>.rs` をクレートへ入れるには通常 `main.rs` の `mod` 一覧へ
//! 1 行足す必要があり、**それはまさに並列ブランチが取り合う共有行**である。
//! `#[path]` を使うと、`main.rs` を 1 バイトも触らずに実体を
//! `crate::features::train::imp` として取り込める (`semconf` と同じ形)。
//!
//! 実体が要求する共有面は**ゼロ**: `app.rs` / `palette.rs` / `feature.rs` /
//! `config.rs` / `keybinds.rs` / `main.rs` / `build.rs` / `cli.rs` の
//! どれも触っていない。
//!
//! ## 姉妹機能との住み分け
//!
//! * `lease.rs` — 同じファイルを 2 人に触らせない (**起こさない**側)
//! * `conflict.rs` — 近い行の衝突を先に見せる (**見せる**側)
//! * `semconf.rs` — ファイルが違うのに噛み合わない変更を見せる
//! * `train.rs` — 見つけた後に**順番を決めて実際に統合する**側。
//!   `RadarAction` が `Open` / `Close` の 2 つしか無く空白だったところ。
//!
//! ## 統合担当への申し送り
//!
//! CLI (`zai train plan|run`) の入口は [`cli_main`] として公開してある。
//! `src/cli.rs` は共有ファイルなので**こちらでは配線していない** —
//! サブコマンドの分岐へ次の 1 行を足すと繋がる:
//!
//! ```ignore
//! "train" => return Some(crate::features::train::cli_main(&args[1..])),
//! ```

#[path = "../train.rs"]
mod imp;

pub use imp::FEATURE;

/// `zai train <sub>` の入口。**`src/cli.rs` は共有ファイルなので、こちらでは
/// 配線していない** — 統合担当が上の 1 行を足すまで呼び出し元が無いので、
/// 未使用であることを明示的に許す。中身は `train.rs` のテストが実 git
/// リポジトリを作って通している (計画・実行・終了コードまで)。
#[allow(unused_imports)]
pub use imp::cli_main;
