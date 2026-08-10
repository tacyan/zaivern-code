//! 🛡 ガード (ベンダー非依存の書き込み強制) の**登録だけ**。実体は `src/guard.rs`。
//!
//! このファイルを置くだけで機能が繋がる。`main.rs` の `mod` 一覧にも
//! `feature.rs` のレジストリにも触らない (build.rs が集める)。
//!
//! 定義を写経して 2 か所に持つとズレるので、ここは**再エクスポートするだけ**。

#[path = "../guard.rs"]
mod imp;

pub use imp::{cli_main, FEATURE};
