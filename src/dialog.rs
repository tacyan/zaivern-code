//! ダイアログの枠を **1 箇所**で決める。
//!
//! ## なぜ要るか (実測)
//!
//! `egui::Window` を素で使うと、**中身が窓より高いときに画面の外へ出る**。
//! `Window::max_height` を足しても直らない — あれは中身 (`Resize`) の上限で
//! あって、タイトルバー + 枠の余白 (実測 **44.7px**) は数に入らないからである。
//!
//! `src/e2e.rs` の `probe::窓枠の実測` で、60 行の中身を 1200×300 の窓へ
//! 描いたときの結果:
//!
//! | 作り                                        | 窓の縦 | 位置        |
//! |---------------------------------------------|-------:|-------------|
//! | 素の `Window`                               | 1358.7 | −529 〜 830 |
//! | 中身を `ScrollArea` で包む                  |  312.0 | **−6 〜 306** |
//! | `ScrollArea` + `constrain_to(画面 − 余白)`  |  296.0 | 2 〜 298 ✅ |
//!
//! 真ん中の段が曲者で、**ほぼ収まっているのに 6px だけ上下へ出る**。
//! 出た 6px にタイトルバーの上端 (= 掴んで動かす場所) と `✕` が乗るので、
//! 「閉じられない・動かせないダイアログ」になる。
//!
//! ## 使い方
//!
//! ```ignore
//! crate::dialog::window(ctx, tr("タイトル"))
//!     .open(&mut open)
//!     .show(ctx, |ui| crate::dialog::scroll_body(ui, "一意な名前", |ui| { … }));
//! ```
//!
//! 両方要る。`constrain_to` だけでは中身が縮まないので、はみ出た窓が
//! 画面の端へ寄るだけになる。

/// 画面の縁とダイアログの間に必ず残す余白 (px)。
pub const SCREEN_MARGIN: f32 = 8.0;

/// **画面から出ない**ダイアログの枠。
///
/// `egui::Window::new` の代わりに呼ぶ。`open` / `collapsible` /
/// `resizable` / `anchor` などは戻り値へ続けて足す。
pub fn window<'o>(ctx: &egui::Context, title: impl Into<egui::WidgetText>) -> egui::Window<'o> {
    egui::Window::new(title).constrain_to(ctx.screen_rect().shrink(SCREEN_MARGIN))
}

/// 窓枠が**横**に食う幅 (実測 12px: 左右の余白 + 枠線)。
const CHROME_W: f32 = 12.0;

/// 「これくらい欲しい」幅を、画面に収まる範囲へ丸める。
///
/// `ui.set_min_width(420.0)` のような固定値は、狭い窓 (360px) で
/// **横に −36 〜 396 とはみ出す**。欲しい幅は素直に書いて、ここを通す。
pub fn body_width(ctx: &egui::Context, want: f32) -> f32 {
    let room = ctx.screen_rect().width() - SCREEN_MARGIN * 2.0 - CHROME_W;
    want.min(room.max(160.0))
}

/// ダイアログ本体を縦スクロールで包む。
///
/// `salt` は同時に開きうる他のダイアログと**必ず違う**文字列にすること
/// (`ScrollArea` は `make_persistent_id` を通るので、被ると片方の
/// スクロール位置がもう片方に乗り移る)。
pub fn scroll_body<R>(ui: &mut egui::Ui, salt: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::ScrollArea::vertical()
        .id_salt(salt)
        .show(ui, add)
        .inner
}

#[cfg(test)]
mod tests {
    /// 余白は正の値でなければ意味が無い (0 だと縁に貼り付く)。
    #[test]
    fn 画面の縁との余白は正の値() {
        assert!(super::SCREEN_MARGIN > 0.0);
    }

    /// **この助けを使っている呼び出しが実在する**ことを構造で固定する。
    /// (「作ったのに繋いでいない」の検出器。改行は正規化する。)
    #[test]
    fn ダイアログの助けは実際に使われている() {
        for (name, src) in [
            (
                "local_history.rs",
                include_str!("local_history.rs").replace("\r\n", "\n"),
            ),
            (
                "agent_picker.rs",
                include_str!("agent_picker.rs").replace("\r\n", "\n"),
            ),
            (
                "orchestration.rs",
                include_str!("orchestration.rs").replace("\r\n", "\n"),
            ),
        ] {
            assert!(
                src.contains("crate::dialog::window("),
                "{name} が crate::dialog::window を使っていない"
            );
        }
    }
}
