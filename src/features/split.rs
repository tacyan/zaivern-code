//! 事前分割 (配る前に、衝突し得ない担当表を作る) の**登録だけ**。
//! 実体は `src/split.rs`。
//!
//! このファイルを置くだけで機能が繋がる。`main.rs` の `mod` 一覧にも
//! `feature.rs` のレジストリにも触らない (build.rs が集める)。
//!
//! `cli_main` も一緒に出しておく — `zai split …` の配線は
//! `src/cli.rs` (並列ブランチが取り合う共有ファイル) なので、
//! **統合担当が直列に 1 行入れる**約束になっている。

#[path = "../split.rs"]
mod imp;

// `zai split …` は `src/cli.rs` の dispatch から呼ばれる (統合時に直列で配線済み)。
pub use imp::{cli_main, FEATURE};
