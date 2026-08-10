//! 🛰 衝突レーダー (並列ワークツリーのマージ衝突予測) の**登録だけ**。
//! 実体は [`crate::conflict`]。
//!
//! このファイルを置くだけで機能が繋がる。`main.rs` の `mod` 一覧にも
//! `feature.rs` のレジストリにも触らない (build.rs が集める)。
//!
//! **このレジストリ自体が、このモジュールの姉妹対策**である —
//! `conflict.rs` は「起きてしまう衝突を、まだ安いうちに見せる」側、
//! レジストリは「共有行を無くして衝突をそもそも起こさない」側で、
//! 並列エージェント開発には両方が要る。

/// コマンドパレットからの到達経路。
///
/// 打鍵は割り当てていない。パレットと Cockpit ヘッダのバッジで既に 2 経路あり、
/// CLAUDE.md の「同じ操作への到達経路が 3 つあるなら 2 つ削る」に従う。
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "conflict",
    entries: &[crate::feature::Entry {
        icon: "🛰",
        label: "衝突レーダー — 並列ワークツリーのマージ衝突を先に見る",
        id: "conflict.open",
    }],
    dispatch: |app, _ctx, id| match id {
        "conflict.open" => {
            app.toggle_conflict_radar();
            true
        }
        _ => false,
    },
    // 窓は中央ビューに属さないオーバーレイなので、毎フレームここから描く。
    // **閉じているときは 1 命令も走らない** (`conflict_radar_ui` の先頭で
    // 即 return する) ので、アイドル時のコストはゼロのまま。
    draw: Some(|app, ctx| app.conflict_radar_ui(ctx)),
    ..crate::feature::Feature::DEFAULT
};
