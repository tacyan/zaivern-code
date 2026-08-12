use super::*;

/// 隔離されたパネルは**何も描かずに return しない**。
///
/// パネルごと消すと egui はその矩形へ何も塗らず、直前のフレームの
/// ピクセル (= ウィンドウ背景 = 黒) が残り続ける。しかも利用者には
/// 何が起きたのか分からない。ここが `return;` に戻ったら気付けるように。
#[test]
fn 隔離されたパネルは代わりの説明を描く() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn guarded_view(&mut self, sv: Subview, ctx: &egui::Context, draw:")
        .nth(1)
        .expect("guarded_view がある");
    let head = &body[..body.find("\n    /// ").unwrap_or(body.len())];
    assert!(
        head.contains("self.quarantined_panel_ui(&sv, ctx);"),
        "隔離時に代わりの描画をしていない (黒い空間になる)"
    );
    // guarded_ui 側も同じ約束
    let ui_body = src
        .split("fn guarded_ui(")
        .nth(1)
        .expect("guarded_ui がある");
    let ui_head = &ui_body[..ui_body.find("\n    /// ").unwrap_or(ui_body.len())];
    assert!(ui_head.contains("quarantine_placeholder_ui"));
}

/// 代わりの描画は**生きているときと同じパネル id** を使う。
/// id を変えるとリサイズ幅の記憶が失われ、復帰した瞬間に既定幅へ戻る。
#[test]
fn 代わりのパネルは同じidを使う() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn quarantined_panel_ui(")
        .nth(1)
        .expect("代わりの描画がある");
    let head = &body[..body.find("\n    /// ").unwrap_or(body.len())];
    assert!(
        head.contains("SidePanel::left(\"zv-side\")"),
        "サイドバーの id が違う"
    );
    assert!(
        head.contains("TopBottomPanel::bottom(\"zv-terminal\")"),
        "ターミナルパネルの id が違う"
    );
    // 想定外の Subview でも画面を黒くしない受け皿がある
    assert!(head.contains("CentralPanel::default()"));
}

/// プレースホルダには復帰手段 (再試行) がある。
/// バナーは 1 度閉じると出ないので、ここに無いと永久に死んだままになる。
#[test]
fn プレースホルダから再試行できる() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn quarantine_placeholder_ui(")
        .nth(1)
        .expect("プレースホルダがある");
    let head = &body[..body.find("\n    /// ").unwrap_or(body.len())];
    assert!(head.contains("-> bool"), "押されたことを返していない");
    assert!(head.contains("ここだけ再試行"), "再試行ボタンが無い");
    // 押したら「その Subview だけ」解除する
    for f in ["fn guarded_ui(", "fn quarantined_panel_ui("] {
        let b = src.split(f).nth(1).expect("あるはず");
        let h = &b[..b.find("\n    /// ").unwrap_or(b.len())];
        assert!(h.contains("unquarantine("), "{f} が個別解除を呼んでいない");
    }
}

/// 個別解除は「その 1 つだけ」外し、頻度の記憶もまっさらにする。
/// 記憶が残ると「再試行 → 1 回 panic → 即また隔離」で直らないように見える。
#[test]
fn 個別解除はその一つだけを外して記憶も消す() {
    let mut g = FrameGuard::default();
    g.quarantined.insert(Subview::Panel("sidebar"));
    g.quarantined.insert(Subview::Session(7));
    g.banner = Some("dummy".into());
    // panic の記憶を積んでおく
    g.policy.streak = 2;
    g.policy.recent = vec![1, 2, 3];
    g.policy.clean = 0;

    g.unquarantine(&Subview::Panel("sidebar"));
    assert!(
        !g.is_quarantined(&Subview::Panel("sidebar")),
        "外れていない"
    );
    assert!(g.is_quarantined(&Subview::Session(7)), "他まで外している");
    assert_eq!(g.policy.streak, 0, "連続カウンタが残っている");
    assert!(g.policy.recent.is_empty(), "時間窓の記憶が残っている");
    assert!(
        g.banner.is_some(),
        "まだ隔離が残っているのでバナーは消さない"
    );

    // 最後の 1 つを外したらバナーも消える
    g.unquarantine(&Subview::Session(7));
    assert!(g.quarantined.is_empty());
    assert!(g.banner.is_none(), "隔離が空なのにバナーが残っている");
}

/// 消えたセッションの隔離は掃く。
/// 残すと ID が再利用されたとき、新しいタイルがいきなり黒く出る。
#[test]
fn 消えたセッションの隔離は掃かれる() {
    let mut g = FrameGuard::default();
    g.quarantined.insert(Subview::Session(1));
    g.quarantined.insert(Subview::Session(2));
    g.quarantined.insert(Subview::Panel("editor"));

    let alive: HashSet<u64> = [2u64].into_iter().collect();
    g.forget_sessions(&alive);

    assert!(
        !g.is_quarantined(&Subview::Session(1)),
        "消えたセッションが残っている"
    );
    assert!(
        g.is_quarantined(&Subview::Session(2)),
        "生きている方まで外している"
    );
    assert!(
        g.is_quarantined(&Subview::Panel("editor")),
        "パネルの隔離まで外している"
    );

    // 全部消えたらバナーも消す
    g.banner = Some("dummy".into());
    g.quarantined.clear();
    g.quarantined.insert(Subview::Session(9));
    g.forget_sessions(&HashSet::new());
    assert!(g.quarantined.is_empty());
    assert!(g.banner.is_none());
}

/// 掃除はセッションの増減を拾う 1 か所 (`reconcile_sessions`) で走る。
#[test]
fn 隔離の掃除はセッション増減の場所で走る() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn reconcile_sessions(&mut self) {")
        .nth(1)
        .expect("あるはず");
    let head = &body[..body.find("\n    /// ").unwrap_or(body.len())];
    assert!(
        head.contains("self.frame_guard.forget_sessions(&live);"),
        "隔離集合を掃いていない"
    );
}
