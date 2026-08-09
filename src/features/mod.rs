//! 機能の登録置き場 — **ここへファイルを 1 つ足すだけで機能が繋がる。**
//!
//! ## 約束
//!
//! * 機能を足す人が触るのは **`src/features/<名前>.rs` という新規ファイル 1 つだけ**。
//!   `main.rs` の `mod` 一覧にも、`feature.rs` のレジストリにも触らない。
//! * `mod` 宣言と一覧は [`build.rs`] が `src/features/*.rs` を走査して生成し、
//!   `OUT_DIR` へ置く。生成物はコミットしないので、生成物自体も衝突しない。
//! * ファイル名がそのままモジュール名になる。小文字・数字・`_` のみ、先頭は小文字。
//!
//! ## なぜこの形なのか
//!
//! **git が衝突を作るのは「2 つのブランチが同じファイルの近い行を触った」時だけ**。
//! 共有ファイルへの追記が 1 行でも残る限り、同時に足せば必ず衝突する
//! (実際に which-key と local_history が `config.rs` の設定一覧で衝突した)。
//! 追記そのものを無くせば、衝突は**構造的に起こり得ない**。
//!
//! ## 書き方
//!
//! ```ignore
//! // src/features/marks.rs
//! use crate::feature::{Entry, Feature};
//!
//! pub const FEATURE: Feature = Feature {
//!     module: "marks",
//!     entries: &[Entry { icon: "🔖", label: "ブックマーク一覧", id: "marks.list" }],
//!     dispatch: |app, _ctx, id| match id {
//!         "marks.list" => { app.open_marks(); true }
//!         _ => false,
//!     },
//!     draw: None,
//!     settings: &[],
//! };
//! ```
//!
//! 機能の実体は `src/<名前>.rs` に置いたままでよい (このファイルは登録だけ)。
//! `ZaivernApp` の内部へ触る必要があるときは `app.rs` 側に `pub(crate)` の
//! メソッドを 1 つ出す — フィールドを公開しない。

include!(concat!(env!("OUT_DIR"), "/features_generated.rs"));
