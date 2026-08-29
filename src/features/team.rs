//! 🏛 AI 開発チーム制御面 (Team) の**登録だけ**。実体は `src/team/`。
//!
//! `#[path]` で実体を引き込むので `main.rs` の `mod` 一覧を 1 バイトも
//! 触らない (`coedit` / `train` / `split` と同じ形)。

#[path = "../team/mod.rs"]
pub mod imp;

/// `zai team <sub>` の入口。`src/cli.rs` の dispatch から呼ばれる。
pub use imp::cli::cli_main;

/// `zai team run` が「CLI ではなく GUI を起こしてほしい」と伝える値。
pub use imp::cli::EXIT_LAUNCH_GUI;

/// `zai help` に差し込むセクション。**実体は 1 か所** (`src/team/cli.rs`)。
pub use imp::cli::HELP;

pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "team",
    entries: &[],
    dispatch: |_app, _ctx, _id| false,
    ..crate::feature::Feature::DEFAULT
};
