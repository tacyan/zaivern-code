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

// `zai split …` の配線先は `src/cli.rs` — 並列ブランチが取り合う共有ファイルなので、
// **統合担当が直列に 1 行**入れる約束になっている (CLAUDE.md「ゼロにできていない共有面」)。
// それまで `cli_main` を呼ぶ人が居ないので unused_imports が出るが、消すと
// 再エクスポートごと消えて配線先が無くなる。**繋ぐまでの間だけ**許す。
#[allow(unused_imports)]
pub use imp::{cli_main, FEATURE};
