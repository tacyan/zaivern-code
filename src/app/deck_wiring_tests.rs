/// デッキ表示中は下部ターミナルパネルを畳む。
#[test]
fn デッキ表示中は端末パネルを出さない() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn terminal_panel(&mut self, ctx: &egui::Context) {")
        .nth(1)
        .expect("terminal_panel がある");
    let head = &body[..body.find("let panel =").unwrap_or(body.len())];
    // 生のフラグではなく**このフレームの中央ビュー**で判断する。
    // Editor のときだけ出す = デッキ/Cockpit/看板では畳む。
    assert!(
        head.contains("let show = self.agents.panel_open && self.center == CenterView::Editor;"),
        "デッキを開いている間も端末パネルが出る配線になっている"
    );
}

/// デッキの端末描画は Cockpit / 看板と同じ隔離印を通す。
#[test]
fn デッキの端末描画はフレームガードを通す() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn deck_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {")
        .nth(1)
        .expect("deck_ui がある");
    let head = &body[..body.find("for act in acts {").unwrap_or(body.len())];
    assert!(head.contains("draw_subview(Subview::Session(id)"));
    assert!(head.contains("terminal::draw("));
    // PTY の読み直しはサンプリング周期のフレームだけ
    assert!(head.contains("self.deck_state.sample_due(now_ms)"));
    assert!(head.contains("if fresh_tail {"));
}

/// deck.rs は無条件の `request_repaint` を持たない
/// (予約は `deck_repaint_ms` が `Some` を返したときだけ)。
#[test]
fn デッキは無条件の再描画予約を持たない() {
    let src = &include_str!("../deck.rs").replace("\r\n", "\n");
    assert!(
        !src.contains("request_repaint()"),
        "deck.rs に無条件の request_repaint がある (アイドル時の CPU が跳ねる)"
    );
    assert!(src.contains("if let Some(ms) = deck_repaint_ms("));
}
