//! 🏛 AI 開発チーム制御面 (Team) の**登録だけ**。実体は `src/team/`。
//!
//! `#[path]` で実体を引き込むので `main.rs` の `mod` 一覧を 1 バイトも
//! 触らない (`coedit` / `train` / `split` と同じ形)。

#[path = "../team/mod.rs"]
pub mod imp;

pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "team",
    entries: &[],
    dispatch: |_app, _ctx, _id| false,
    ..crate::feature::Feature::DEFAULT
};
