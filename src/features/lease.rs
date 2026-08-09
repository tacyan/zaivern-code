//! ファイル所有リース (並列エージェントの衝突を発生させない) の**登録だけ**。
//! 実体は [`crate::lease`]。
//!
//! このファイルを置くだけで機能が繋がる。`main.rs` の `mod` 一覧にも
//! `feature.rs` のレジストリにも触らない (build.rs が集める)。

/// コマンドパレットからの到達経路。
///
/// 打鍵は割り当てていない (`keybinds::BindAction` は固定長配列を持つ最も硬い
/// 共有面で、機能ブランチ側から増やすと直列マージが必ず衝突する)。
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "lease",
    entries: &[crate::feature::Entry {
        icon: "🔐",
        label: "ファイル所有の一覧 — 並列エージェントの衝突を防ぐ",
        id: "lease.list",
    }],
    dispatch: |_app, _ctx, id| match id {
        "lease.list" => {
            crate::lease::open_panel();
            true
        }
        _ => false,
    },
    // パネルはウィンドウとして自分で描く (`app.rs` のビュー列挙に触らない)。
    draw: Some(crate::lease::draw),
};
