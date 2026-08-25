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
    // **デッキは PTY を 1 バイトも読まない。**
    // 状態もサンプリング周期も `FleetStore` が持つので、ここで読むと
    // 同じフレームで parser を二重にロックすることになる。
    assert!(
        head.contains("self.fleet.snapshot()"),
        "デッキが Fleet のスナップショットを読んでいない"
    );
    assert!(
        !head.contains("screen_tail_lines("),
        "デッキが PTY を読み直している (二重解析)"
    );
    assert!(
        !head.contains("sample_due("),
        "デッキが自前のサンプリング周期を持っている"
    );
}

/// **PTY 画面を読み直すのは `fleet_tick` 1 か所だけ。**
///
/// 読み手が増えると、同じフレームで parser のロックを何度も取ることになり、
/// しかも「どのタイミングの画面か」が読み手ごとにずれる。
#[test]
fn 画面末尾を読むのはfleet_tickだけ() {
    let src = &crate::app::SRC_IMPL.replace("\r\n", "\n");
    let body = src
        .split("pub(super) fn fleet_tick(&mut self) {")
        .nth(1)
        .expect("fleet_tick がある");
    let head = &body[..body.find("\n    }").unwrap_or(body.len())];
    // 間引きを通してからしか読まない
    assert!(
        head.contains("self.fleet.sample_due(now_ms)"),
        "fleet_tick が間引きを通さずに PTY を読んでいる"
    );
    assert!(
        head.contains("fresh.then(|| s.screen_tail_lines("),
        "画面末尾の読み取りが間引きに掛かっていない"
    );
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
/// 枠がパーサより低いぶんは [`terminal::grid_rect`] が「生きている最下行を
/// 映す」形で吸収するので、見え方は変わらない。
///
/// **縦と横で規則が違う** ([`terminal::next_pty_size`] に 1 本だけ置く):
///
/// * 縦 — ライブ枠は**広げるだけ**。以前は「主がまだ現れていない間だけ」という
///   一方向のラッチ (`size_owned`) を挟んでいたが、それだと**一度でも下部パネルが
///   低い高さで描いた端末は、その大きさに永久に固定される**。Cockpit を全画面で
///   開いても小さいままなので、枠の上 3/4 が空白になる
///   (「途中で全画面表示されなくなる」として報告された)。ラッチが守っていたはずの
///   再送は実 PTY で計ると **動かさない 0 回 / 縮める 0 回 / 広げる 0 回**
///   (claude 2.1.234) で再現しなかった — Ink の静的出力は一度きりだから。
/// * 横 — **必ずいま描いている枠に合わせる**。看板のライブペインは「全画面
///   (窓幅いっぱい)」と「分割 (細い)」の 2 通りの幅を持つので、持ち越しを許すと
///   **看板を出していないときの幅**が Cockpit や下部パネルへ残り、文字が右へ
///   見切れる (利用者からの報告)。見切れは UI の原則に反するので持ち越さない。
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
    // **縦**はライブ枠から広げるだけ (縮める経路は主 `allow_resize` だけ)。
    // **横**は必ずいま描いている枠に合わせる — 他のビューの幅を持ち越すと
    // 文字が右へ見切れる (看板のライブペインは全画面と分割で幅が 2 通りある)。
    assert!(
        term.contains("let (r, c) = next_pty_size(session.size, (rows, cols), allow_resize);"),
        "大きさの規則が純関数を通っていない (表で固定できない = 静かにずれる)"
    );
    assert!(
        term.contains("pub fn next_pty_size("),
        "大きさの規則が 1 か所に無い"
    );
    // 一方向のラッチを戻さない (戻すと、小さい高さで一度描いた端末が
    // 全画面のタイルでも小さいまま残り、枠の上が空白になる)。
    //
    // **見るのは `draw` の中だけ・コメント行は除く。** 範囲を広げると経緯を
    // 書いた文章を拾って空回りする (`cli::tests::改行をまたぐ照合…` と同じ轍)。
    let draw_body = term
        .split_once("\npub fn draw(")
        .expect("terminal::draw がある")
        .1;
    let draw_body = &draw_body[..draw_body
        .find("\n    let grid = {")
        .unwrap_or(draw_body.len())];
    let code_only: String = draw_body
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !code_only.contains("size_owned"),
        "広げる経路にラッチが戻っている (全画面のタイルで端末が小さいまま残る)"
    );
    // 広げる判断は毎フレーム通ること (条件を足すと、また固定される)。
    assert!(
        code_only.contains("    {\n        let cols = ((rect.width() - padding * 2.0) / cell_w)"),
        "リサイズの判断に条件が戻っている (ライブ枠が広げられなくなる)"
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
