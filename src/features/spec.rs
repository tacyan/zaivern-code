//! spec 駆動開発 (デルタ形式 + 陳腐化検出) の**登録だけ**。実体は [`crate::spec`]。
//!
//! このファイルを置くだけで機能が繋がる。`main.rs` の `mod` 一覧にも
//! `feature.rs` のレジストリにも触らない (build.rs が集める)。

/// コマンドパレットからの到達経路。
///
/// 打鍵は割り当てていない。パレットとボトムパネルのタブで既に 2 経路あり、
/// CLAUDE.md の「同じ操作への到達経路が 3 つあるなら 2 つ削る」に従う。
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "spec",
    entries: &[
        crate::feature::Entry {
            icon: "📐",
            label: "Spec — 仕様の差分と陳腐化を見る",
            id: "spec.open",
        },
        crate::feature::Entry {
            icon: "⚠",
            label: "陳腐化した仕様だけを一覧する",
            id: "spec.stale",
        },
    ],
    dispatch: |app, _ctx, id| match id {
        "spec.open" => {
            app.open_spec_panel();
            true
        }
        "spec.stale" => {
            app.open_spec_stale();
            true
        }
        _ => false,
    },
    // パネルはボトムパネル側で描くので、全画面オーバーレイは持たない。
    draw: None,
    ..crate::feature::Feature::DEFAULT
};
