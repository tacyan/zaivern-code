//! 🏛 AI 開発チーム制御面 (Team) の**登録だけ**。実体は `src/team/`。
//!
//! `#[path]` で実体を引き込むので `main.rs` の `mod` 一覧を 1 バイトも
//! 触らない (`coedit` / `train` / `split` と同じ形)。
//!
//! ## この機能が既存へ求める面
//!
//! * `src/cli.rs` — `zai team <sub>` の 1 行 (統合時に配線済み)
//! * `src/app/team_glue.rs` — 既存の起動・送信・停止・端末選択へ繋ぐ橋。
//!   `launch_preset` / `queue_submit` / `close_agent` / `focus_agent_in_place`
//!   はどれも `pub(super)` で `crate::app` の中からしか呼べないため、
//!   グルーは `src/app/` に置くしかない (CLAUDE.md が認めている例外)。
//!   **`ZaivernApp` に欄は 1 つも増やしていない** — Team の状態は
//!   `imp::panel` の `thread_local!` が持つ。

#[path = "../team/mod.rs"]
pub mod imp;

/// `zai team <sub>` の入口。`src/cli.rs` の dispatch から呼ばれる。
pub use imp::cli::cli_main;

/// `zai team run` が「CLI ではなく GUI を起こしてほしい」と伝える値。
pub use imp::cli::EXIT_LAUNCH_GUI;

/// `zai help` に差し込むセクション。**実体は 1 か所** (`src/team/cli.rs`)。
pub use imp::cli::HELP;

/// コマンドパレットからの到達経路。
///
/// **到達経路は 2 つに絞る** (CLAUDE.md: 同じ操作への経路が 3 つあるなら
/// 2 つ削る)。ここ (パレット) と `zai team run` の 2 本で、打鍵は
/// 割り当てていない。
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "team",
    entries: &[
        crate::feature::Entry {
            icon: "🏛",
            label: "Team — AI 開発チームの組織図を開く",
            id: "team.open",
        },
        crate::feature::Entry {
            icon: "🆕",
            label: "New Team Run — SPEC を渡してチームを編成する",
            id: "team.new_run",
        },
    ],
    dispatch: |app, _ctx, id| match id {
        "team.open" => {
            app.toggle_team_board();
            true
        }
        "team.new_run" => {
            app.open_team_new_run();
            true
        }
        _ => false,
    },
    // **毎フレームここから駆動する。** Run が無いときは 1 命令も走らないので
    // アイドルのコストはゼロ (設計原則 3)。
    draw: Some(|app, ctx| app.team_tick(ctx)),
    ..crate::feature::Feature::DEFAULT
};
