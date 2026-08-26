//! 🧠 コンテキストエンジン (AI へ渡す前に情報量を減らす層) の**登録だけ**。
//! 実体と `FEATURE` の定義は [`crate::context`] にある。
//!
//! 定義を写経して 2 か所に持つとズレるので、ここは**再エクスポートするだけ**。
//! (`src/features/jump.rs` と同じ形。実体が 1 ファイルではなくディレクトリ
//!  なので、`#[path]` ではなく `main.rs` の `mod context;` から生やしている
//!  — こうしておくと `crate::context::…` が**クレート内 API**として
//!  他のモジュールからも呼べる。将来 Scheduler / Provider 層が使う面がここ。)

pub use crate::context::{cli_main, FEATURE, HELP};
