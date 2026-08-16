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

/// **PTY の大きさを持つビューは 1 つだけ。**
///
/// デッキ / 看板 / Cockpit の小さなライブ枠まで PTY を縮めると、Ink 系の
/// CLI エージェント (Claude Code など) は**枠より高いフレームを丸ごと
/// 描き直す**ので、同じ会話が履歴へもう一度積まれる。端末しか見ていない
/// 利用者には「コピーすると同じ文章が 2 回入る」「デッキへ行って戻ると
/// 2 重に見える」として現れる。
///
/// 実測 (claude 2.1.233 の実ログ 55KB を再生し、同じ 1 行が履歴に何回
/// 並ぶかを数えた): **58 行 → 1 回 / 45 行 → 1 回 / 40 行 → 2 回 /
/// 30 行 → 2 回 / 24 行 → 4 回**。縮めるほど増える。
///
/// 枠がパーサより低いぶんは [`terminal::grid_rect`] が「いちばん下 (最新) を
/// 映す」形で吸収するので、見え方は変わらない。
#[test]
fn ライブ枠は端末の大きさを持たない() {
    // **実装部だけ**を読む (`SRC` はテストも含むので、この検査自身の
    // 文字列を数えてしまう)。
    let src = &crate::app::SRC_IMPL.replace("\r\n", "\n");
    // `terminal::draw(ui, s, theme, font, interactive, allow_resize, hover_scroll)`
    // の 6 番目 (allow_resize) が true なのは、主たる端末ビューだけ。
    let owners = src.matches("let resp = terminal::draw(ui, s, &theme, font, true, true, true);");
    assert_eq!(owners.count(), 1, "端末の大きさを持つビューが 1 つではない");
    for preview in [
        // Cockpit のミニ端末グリッド
        "terminal::draw(\n                                            ui, s, theme, mini_font, true, false, false,\n                                        )",
        // Cockpit の分割タイル
        "terminal::draw(ui, s, theme, mini_font, true, false, false);",
        // 看板 / デッキのライブ枠 (2 か所とも同じ形)
        "terminal::draw(ui, s, &live_theme, mini_font, true, false, false)",
    ] {
        assert!(
            src.contains(preview),
            "プレビュー枠が PTY の大きさを持ったままになっている:\n{preview}"
        );
    }
    // 縮めない代わりに「最新を映す」側が居ること
    let term = include_str!("../terminal.rs").replace("\r\n", "\n");
    // 主が居ない間だけ**広げる**のは許す (デッキ起動の端末が小さいまま
    // 取り残されないため)。縮める経路は主 (`allow_resize`) にしか無い。
    assert!(
        term.contains("if allow_resize || !session.size_owned {"),
        "ライブ枠が端末を広げる経路が無い (デッキ起動の端末が取り残される)"
    );
    assert!(
        term.contains("session.resize(rows.max(cur_r), cols.max(cur_c));"),
        "ライブ枠が端末を縮められてしまう"
    );
    assert!(
        term.contains("session.size_owned = true;"),
        "主が大きさを主張していない (以後もライブ枠に触られる)"
    );
    assert!(
        term.contains("pub fn grid_rect("),
        "枠より高いグリッドの受け皿 (grid_rect) が無い"
    );
    assert!(
        term.contains("let grid = {"),
        "draw が grid_rect を使っていない"
    );
    for user in [
        "handle_mouse_selection(session, &response, grid, padding, cell_w, cell_h);",
        "handle_wheel_scroll(ui, session, grid, padding, cell_w, cell_h);",
        "ui, &painter, session, theme, &font_id, grid, padding, cell_w, cell_h, focused,",
        "draw_shell_decorations(ui, session, theme, grid, padding, cell_w, cell_h);",
        "paint_search_highlights(&painter, session, theme, grid, padding, cell_w, cell_h);",
    ] {
        // 描画と当たり判定が別の矩形を見ると、**コピーだけが静かにずれる**
        assert!(
            term.contains(user),
            "グリッド座標を使っていない経路がある:\n{user}"
        );
    }
}
