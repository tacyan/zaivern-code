//! 🧭 意味的衝突の検出 (ファイルは違うのに噛み合わない変更) の**登録だけ**。
//! 実体は `src/semconf.rs`。
//!
//! このファイルを置くだけで機能が繋がる。`feature.rs` のレジストリにも
//! `build.rs` にも触らない (build.rs が `src/features/*.rs` を走査して拾う)。
//!
//! ## なぜ `#[path]` で実体を引き込むのか
//!
//! 実体は CLAUDE.md の指示どおり `src/semconf.rs` に置いている。ただし
//! `src/<名前>.rs` をクレートへ入れるには通常 `main.rs` の `mod` 一覧へ
//! 1 行足す必要があり、**それはまさに並列ブランチが取り合う共有行**である。
//! ここで `#[path]` を使うと、`main.rs` を 1 バイトも触らずに実体を
//! `crate::features::semconf::imp` として取り込める。
//!
//! 実体が要求する共有面は**ゼロ**: `app.rs` / `palette.rs` / `feature.rs` /
//! `config.rs` / `keybinds.rs` / `main.rs` / `build.rs` のどれも触っていない。
//!
//! ## 姉妹機能との住み分け
//!
//! * `lease.rs` — 同じファイルを 2 人に触らせない (**起こさない**側)
//! * `conflict.rs` — 同じファイルの近い行の衝突を先に見せる (**早く見せる**側)
//! * `semconf.rs` — **ファイルが違うのに**噛み合わない変更を見せる。
//!   リースも git も通してしまい、マージが成功してビルドだけが壊れる領域。

#[path = "../semconf.rs"]
mod imp;

pub use imp::FEATURE;
