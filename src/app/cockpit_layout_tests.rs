use super::*;

/// 既定のミニターミナル (端末 13pt → ミニ 10pt) での快適な高さ。
/// レイアウトのテストはこの値を基準にする。
fn comfort() -> f32 {
    grid_comfort_cell_h(10.0)
}

/// タイルの総幅は、どんな幅・枚数でも割り当てを超えない。
#[test]
fn グリッドは可用幅からはみ出さない() {
    for w in [320.0_f32, 480.0, 639.0, 640.0, 900.0, 1440.0, 2560.0] {
        for h in [200.0_f32, 600.0, 1200.0] {
            for n in 1..=9usize {
                let g = cockpit_grid_metrics(egui::vec2(w, h), n, comfort());
                let total = g.cell_w * g.cols as f32 + GRID_SPACING * (g.cols as f32 - 1.0);
                assert!(
                    total <= w,
                    "w={w} h={h} n={n}: 総幅 {total} が可用幅 {w} を超えた"
                );
                assert!(g.cell_w > 0.0 && g.cell_h >= GRID_MIN_CELL_H);
                assert!(g.cols * g.rows >= n, "全部のタイルが載らない");
            }
        }
    }
}

/// 狭いときは 1 列へ落ちる (2 列のままだとタイルが潰れて中身が読めない)。
#[test]
fn 狭い窓では一列に落ちる() {
    assert_eq!(
        cockpit_grid_metrics(egui::vec2(639.0, 800.0), 4, comfort()).cols,
        1
    );
    assert_eq!(
        cockpit_grid_metrics(egui::vec2(640.0, 800.0), 4, comfort()).cols,
        2
    );
    // 1 枚しかないときは幅があっても 1 列 (半分空けない)
    assert_eq!(
        cockpit_grid_metrics(egui::vec2(1600.0, 800.0), 1, comfort()).cols,
        1
    );
}

/// **6 枚以上でタイルを潰さない。**
///
/// これが本題: 枚数が増えたら 1 画面へ詰め込むのをやめ、快適な高さを保った
/// ままスクロールへ逃がす。ここが緩むと「開くほど何も読めない」に戻る。
#[test]
fn 枚数が増えてもタイルを潰さずスクロールへ逃がす() {
    let avail = egui::vec2(1280.0, 700.0); // 2 列に割れる普通の窓
    for n in 1..=16usize {
        let g = cockpit_grid_metrics(avail, n, comfort());
        assert!(
            g.cell_h >= comfort() - 0.5,
            "n={n}: タイルが快適な高さ ({}) を割った: {}",
            comfort(),
            g.cell_h
        );
    }
    // 4 枚 (2 行) までは 1 画面に収まり、6 枚 (3 行) からはスクロールする。
    assert!(!cockpit_grid_metrics(avail, 4, comfort()).scrolls(avail.y));
    assert!(cockpit_grid_metrics(avail, 6, comfort()).scrolls(avail.y));
    assert!(cockpit_grid_metrics(avail, 12, comfort()).scrolls(avail.y));
}

/// 枚数が増えても**既存タイルの高さは変わらない** (= 既存 PTY が
/// リサイズされない)。快適な高さで頭打ちになったあとの性質。
#[test]
fn 快適な高さに達したら枚数を足しても高さが動かない() {
    let avail = egui::vec2(1280.0, 700.0);
    let h6 = cockpit_grid_metrics(avail, 6, comfort()).cell_h;
    for n in 6..=20usize {
        assert_eq!(
            cockpit_grid_metrics(avail, n, comfort()).cell_h,
            h6,
            "n={n}: 枚数を足しただけで既存タイルの高さが動いた"
        );
    }
}

/// 低い窓では 1 枚ぶんは窓へ収める (タイル 1 枚しか無いのに
/// スクロールさせない)。窓を縮めていったときに連続であること。
#[test]
fn 低い窓では一枚を窓に収める() {
    for h in [180.0_f32, 240.0, 320.0, 400.0] {
        let g = cockpit_grid_metrics(egui::vec2(900.0, h), 1, comfort());
        assert!(
            !g.scrolls(h),
            "h={h}: タイル 1 枚しか無いのにスクロールしている"
        );
    }
}

/// 快適な高さは文字サイズに追従する (大きい文字ほど高いタイルが要る)。
#[test]
fn 快適な高さは文字サイズに追従する() {
    assert!(grid_comfort_cell_h(14.0) > grid_comfort_cell_h(8.0));
    // どのサイズでも最低高さは割らず、上限も超えない
    for f in [8.0_f32, 10.0, 12.0, 14.0] {
        let h = grid_comfort_cell_h(f);
        assert!((GRID_MIN_CELL_H..=GRID_COMFORT_MAX_H).contains(&h), "f={f}");
    }
}

/// **8 枚のときホイールでページが動く。**
///
/// タイルの中身 (ミニターミナル相当) がセルを埋め尽くしていても、ホイールは
/// 外側の ScrollArea へ抜けて格子全体が動くこと。実際の描画と同じ
/// 「ScrollArea → 行 → セル → 全域を取るクリック可能領域」で組んで確かめる。
#[test]
fn 八枚のときホイールでページがスクロールする() {
    use egui::{pos2, vec2, Rect};

    let ctx = egui::Context::default();
    let screen = vec2(1280.0, 700.0);
    let n = 8usize;
    let g = cockpit_grid_metrics(screen, n, comfort());
    assert!(g.scrolls(screen.y), "前提: 8 枚は 1 画面に収まらない");

    // 1 枚目のセルの矩形を返す (スクロールすれば上へ動く)。
    let draw = |events: Vec<egui::Event>| -> Rect {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), screen)),
            events,
            ..Default::default()
        };
        let mut first = Rect::NOTHING;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("cockpit-grid")
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        for row in 0..g.rows {
                            ui.horizontal(|ui| {
                                for col in 0..g.cols {
                                    let i = row * g.cols + col;
                                    if i >= n {
                                        continue;
                                    }
                                    let cell = ui.scope_builder(
                                        egui::UiBuilder::new()
                                            .id_salt(("cockpit-cell-select", i))
                                            .sense(egui::Sense::click()),
                                        |ui| {
                                            ui.vertical(|ui| {
                                                ui.set_width(g.cell_w - 18.0);
                                                ui.set_height(g.cell_h - 18.0);
                                                // ミニターミナル相当 (フォーカス
                                                // していないのでホイールは取らない)
                                                ui.allocate_exact_size(
                                                    ui.available_size(),
                                                    egui::Sense::click_and_drag(),
                                                );
                                            });
                                        },
                                    );
                                    if i == 0 {
                                        first = cell.response.rect;
                                    }
                                }
                            });
                        }
                    });
            });
        });
        first
    };

    let before = draw(vec![]);
    // 格子の真ん中あたり (= タイルの上) でホイールを回す
    let over = pos2(screen.x * 0.5, screen.y * 0.5);
    let wheel = vec![
        egui::Event::PointerMoved(over),
        egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: vec2(0.0, -400.0),
            modifiers: egui::Modifiers::NONE,
        },
    ];
    let mut after = draw(wheel);
    // egui はスクロールを数フレームに均すので、落ち着くまで空フレームを回す
    for _ in 0..16 {
        after = draw(vec![]);
    }
    assert!(
        after.top() < before.top() - 100.0,
        "ホイールでページが動いていない: {} → {}",
        before.top(),
        after.top()
    );
}

/// Cockpit のミニターミナルは **allow_resize=false / hover_scroll=false** で描く。
///
/// hover_scroll を true にすると、タイルの上でホイールを回したときに端末の
/// 履歴だけが動き、ページがスクロールできなくなる (タイルは画面の大半を
/// 覆っているので、事実上「6 枚目以降が見られない」に戻る)。
///
/// allow_resize は `app::deck_wiring_tests::ライブ枠は端末の大きさを持たない`
/// の側で理由を書いている (小さな枠へ PTY を縮めると、CLI エージェントの
/// 会話が履歴へ二重に積まれる)。
#[test]
fn ミニターミナルはホイールを外側へ譲る() {
    let src = crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn cockpit_grid_ui(")
        .nth(1)
        .expect("cockpit_grid_ui がある");
    let body = body
        .split("fn active_id(")
        .next()
        .expect("後ろに別の関数がある");
    // 改行位置に依存しないよう空白を潰してから照合する
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("mini_font, true, false, false"),
        "Cockpit のミニターミナルは hover_scroll=false (最後の引数) で描くこと"
    );
}

/// 代表的な窓 (900x700 と極端に低い窓を含む)。
fn areas() -> Vec<egui::Rect> {
    [
        (320.0_f32, 240.0_f32),
        (480.0, 320.0),
        (900.0, 700.0),
        (900.0, 180.0), // 極端に低い窓
        (1400.0, 900.0),
        (1720.0, 1148.0),
        (2560.0, 1440.0),
    ]
    .into_iter()
    .map(|(w, h)| egui::Rect::from_min_size(egui::pos2(12.0, 40.0), egui::vec2(w, h)))
    .collect()
}

/// 空状態カードは**必ず可用領域の中**に収まる。
///
/// 旧実装は概算の上詰めで押し下げていたため、狭い窓では起動ボタンが下端を
/// 突き抜けて押せなくなっていた (スクリーンショットで確認済みの不具合)。
#[test]
fn 空状態カードは可用領域からはみ出さない() {
    for avail in areas() {
        for n in 1..=12usize {
            let l = panels::empty_card(avail, n);
            assert!(
                l.card.left() >= avail.left() - 0.01
                    && l.card.right() <= avail.right() + 0.01
                    && l.card.top() >= avail.top() - 0.01
                    && l.card.bottom() <= avail.bottom() + 0.01,
                "avail={avail:?} n={n}: カード {:?} がはみ出した",
                l.card
            );
            assert!(l.cols * l.rows >= n, "起動口が隠れる (cols*rows < {n})");
            // ボタンの総幅はカードの内側に収まる
            let used = l.btn_w * l.cols as f32 + crate::panels::space::SM * (l.cols as f32 - 1.0);
            assert!(
                used <= l.card.width() - crate::panels::space::MD * 2.0 + 0.01,
                "avail={avail:?} n={n}: ボタン列 {used} がカード幅を超えた"
            );
            assert!(l.btn_w > 0.0);
        }
    }
}

/// 高さが足りるときは中央、足りないときは上詰め + スクロール。
#[test]
fn 空状態は中央に置かれスクロールへ逃げる() {
    let avail = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1400.0, 900.0));
    let l = panels::empty_card(avail, 7);
    assert!(!l.scroll, "1400x900 なら 7 個は収まる");
    let top = l.card.top() - avail.top();
    let bottom = avail.bottom() - l.card.bottom();
    assert!(
        (top - bottom).abs() < 1.0,
        "上下の余白が揃っていない: {top} / {bottom}"
    );
    // 低い窓: カードは可用高いっぱい + スクロール
    let low = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 180.0));
    let l2 = panels::empty_card(low, 7);
    assert!(l2.scroll, "入り切らないならスクロールへ逃がす");
    assert!(l2.card.height() <= low.height() + 0.01);
    assert_eq!(l2.card.top(), low.top(), "入り切らないときは上詰め");
}

/// 幅が足りないときはボタンを段組みして、縦の伸びを抑える。
#[test]
fn 空状態のボタンは幅に応じて段組みする() {
    let narrow = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(360.0, 900.0));
    assert_eq!(panels::empty_card(narrow, 7).cols, 1, "狭ければ 1 列");
    // 低くて広い窓では列を増やして高さを稼ぐ
    let wide_low = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1400.0, 320.0));
    let l = panels::empty_card(wide_low, 7);
    assert!(
        l.cols >= 2,
        "低い窓では段組みして縦を詰める (cols={})",
        l.cols
    );
}

/// メディアカードも**必ず可用領域の中**に収まる (空状態カードと同じ不変条件)。
#[test]
fn メディアカードは可用領域からはみ出さない() {
    for avail in areas() {
        for rows in 0..=6usize {
            for buttons in 1..=3usize {
                let l = panels::media_card(avail, rows, buttons);
                assert!(
                    l.card.left() >= avail.left() - 0.01
                        && l.card.right() <= avail.right() + 0.01
                        && l.card.top() >= avail.top() - 0.01
                        && l.card.bottom() <= avail.bottom() + 0.01,
                    "avail={avail:?} rows={rows} buttons={buttons}: カード {:?} がはみ出した",
                    l.card
                );
                assert!(l.btn_w > 0.0, "幅 0 のボタンを作らない");
                // ボタン列はカードの内側に収まる (横並びのときだけ列になる)
                let cols = if l.stack { 1 } else { buttons };
                let used = l.btn_w * cols as f32 + crate::panels::space::SM * (cols as f32 - 1.0);
                assert!(
                    used <= l.card.width() - crate::panels::space::MD * 2.0 + 0.01,
                    "avail={avail:?} rows={rows} buttons={buttons}: ボタン列 {used} が超えた"
                );
            }
        }
    }
}

/// 広い窓では横並び・狭い窓では縦積み。高さが足りなければスクロールへ逃げる。
#[test]
fn メディアカードは狭いとボタンを縦へ積む() {
    let wide = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1200.0, 700.0));
    let l = panels::media_card(wide, 4, 2);
    assert!(!l.stack, "460px のカードなら 2 ボタンは横に並ぶ");
    assert!(!l.scroll, "700px あれば収まる");
    let top = l.card.top() - wide.top();
    let bottom = wide.bottom() - l.card.bottom();
    assert!(
        (top - bottom).abs() < 1.0,
        "上下の余白が揃っていない: {top} / {bottom}"
    );

    let narrow = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(320.0, 700.0));
    assert!(panels::media_card(narrow, 4, 2).stack, "狭ければ縦へ積む");

    let low = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1200.0, 120.0));
    let l3 = panels::media_card(low, 6, 2);
    assert!(l3.scroll, "入り切らないならスクロールへ逃がす");
    assert_eq!(l3.card.top(), low.top(), "入り切らないときは上詰め");
}

/// トップバー右側は、左のメニューバーと重なる前にアイコンだけへ縮退する。
///
/// 900px 幅で「実行 / ターミナル / ヘルプ」の上に「看板」「Cockpit」
/// 「既定:承認」が重なって描かれていた (どちらも読めない)。
#[test]
fn トップバーは狭いとアイコンだけになる() {
    // 900px: メニューバー + アイコン列でも入らないので装飾系を「⋯」へ畳む
    assert_eq!(top_bar_density(900.0), TopBarDensity::Overflow);
    assert!(top_bar_density(900.0).compact());
    assert_eq!(
        top_bar_density(TOP_BAR_LEFT_W + TOP_BAR_RIGHT_ICON_W),
        TopBarDensity::Compact
    );
    assert_eq!(
        top_bar_density(TOP_BAR_LEFT_W + TOP_BAR_RIGHT_W - 1.0),
        TopBarDensity::Compact
    );
    assert_eq!(
        top_bar_density(TOP_BAR_LEFT_W + TOP_BAR_RIGHT_W),
        TopBarDensity::Full
    );
    assert_eq!(
        top_bar_density(1400.0),
        TopBarDensity::Full,
        "1400px はそのまま"
    );
    assert!(!top_bar_density(2560.0).compact());
    // 幅が広がるほど密度が下がることはない (単調)
    let order = |d: TopBarDensity| match d {
        TopBarDensity::Overflow => 0,
        TopBarDensity::Compact => 1,
        TopBarDensity::Full => 2,
    };
    let mut prev = 0;
    for w in [
        0.0_f32, 400.0, 800.0, 1000.0, 1049.0, 1050.0, 1200.0, 2000.0,
    ] {
        let cur = order(top_bar_density(w));
        assert!(cur >= prev, "w={w}: 幅が広いのに密度が下がった");
        prev = cur;
    }
}

/// **中央ビューは常に 1 つだけ。**
///
/// 実際に起きていた不具合: Cockpit のヘッダーで「📋 看板」を押すと、その
/// フレームの途中でフラグが変わり、Cockpit のタイルと看板が重なって描かれた。
/// フラグの組み合わせがどうであれ、描くビューは 1 つに畳む。
#[test]
fn 中央ビューは常に一つだけ() {
    // 単独
    assert_eq!(center_view(false, false, false), CenterView::Editor);
    assert_eq!(center_view(true, false, false), CenterView::Cockpit);
    assert_eq!(center_view(false, true, false), CenterView::Kanban);
    assert_eq!(center_view(false, false, true), CenterView::Deck);
    // 競合してもいずれか 1 つ (デッキ > Cockpit > 看板 > エディタ)
    assert_eq!(center_view(true, true, false), CenterView::Cockpit);
    assert_eq!(center_view(true, true, true), CenterView::Deck);
    assert_eq!(center_view(false, true, true), CenterView::Deck);
    assert_eq!(center_view(true, false, true), CenterView::Deck);
    // 全 8 通りで必ず 1 つの値になる (= 重ねようがない)
    for c in [false, true] {
        for k in [false, true] {
            for d in [false, true] {
                let v = center_view(c, k, d);
                let hits = [
                    v == CenterView::Editor,
                    v == CenterView::Cockpit,
                    v == CenterView::Kanban,
                    v == CenterView::Deck,
                ];
                assert_eq!(hits.iter().filter(|x| **x).count(), 1, "c={c} k={k} d={d}");
            }
        }
    }
}

/// **ボトムパネルの中身も常に 1 つだけ。**
/// 「🛡 承認」「🔌 MCP」「🧩 Skills」を独立した bool で持つ以上、描く直前に畳む。
#[test]
fn ボトムパネルのビューは常に一つだけ() {
    // (承認, MCP, Skills, Spec) → 描くもの
    let table = [
        (false, false, false, false, BottomView::Terminal),
        (true, false, false, false, BottomView::Approvals),
        (false, true, false, false, BottomView::Mcp),
        (false, false, true, false, BottomView::Skills),
        (false, false, false, true, BottomView::Spec),
        // 複数立っていても 1 つ (承認 > MCP > Skills > Spec)
        (true, true, true, true, BottomView::Approvals),
        (false, true, true, true, BottomView::Mcp),
        (false, false, true, true, BottomView::Skills),
    ];
    for (a, m, s, p, want) in table {
        assert_eq!(
            bottom_view(a, m, s, p),
            want,
            "承認={a} MCP={m} Skills={s} Spec={p}"
        );
    }
    // 全 16 通りで必ず 1 つの値になる (= 重ねようがない)
    for a in [false, true] {
        for m in [false, true] {
            for s in [false, true] {
                for p in [false, true] {
                    let v = bottom_view(a, m, s, p);
                    let hits = [
                        v == BottomView::Terminal,
                        v == BottomView::Approvals,
                        v == BottomView::Mcp,
                        v == BottomView::Skills,
                        v == BottomView::Spec,
                    ];
                    assert_eq!(
                        hits.iter().filter(|x| **x).count(),
                        1,
                        "a={a} m={m} s={s} p={p}"
                    );
                }
            }
        }
    }
}

/// ボトムパネルの分岐が**畳んだ値の match** になっている (bool の連鎖に戻さない)。
/// ソースを読む回帰テストなので改行は正規化する (Windows は CRLF)。
#[test]
fn ボトムパネルの分岐は畳んだ値を見る() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    assert!(
        src.contains("let view = bottom_view(")
            && src.contains("self.spec_view,\n                );"),
        "描く直前に 1 つへ畳んでいない"
    );
    assert!(
        src.contains("match view {")
            && src.contains("BottomView::Approvals => {")
            && src.contains("BottomView::Mcp => {")
            && src.contains("BottomView::Skills => {")
            && src.contains("BottomView::Spec => {")
            && src.contains("BottomView::Terminal => {"),
        "ボトムパネルの分岐が match になっていない"
    );
}

/// MCP パネルは**要求されたときだけ**走査する (アイドルのコストをゼロに保つ)。
#[test]
fn mcpの走査はビューを出している間だけ() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    // 「表示中かつ未走査」のガードの**直後**でだけ走査する。毎フレーム
    // `~/.claude.json` (100KB 級) を読むと、アイドルのコストがゼロでなくなる。
    let guard = "if self.mcp_view && !self.mcp.scanned {";
    let body = src.split(guard).nth(1).expect("MCP の走査ガードが無い");
    assert!(
        body.trim_start()
            .starts_with("self.mcp.inventory = mcp::scan("),
        "ガードの直後で走査していない: {:?}",
        body.chars().take(60).collect::<String>()
    );
}

/// Skills パネルも**要求されたときだけ**走査し、UI から到達できる。
///
/// プラグインの木は数百ディレクトリあるので、毎フレーム歩くと
/// アイドルのコストがゼロでなくなる (設計原則 3)。
#[test]
fn skillsの走査はビューを出している間だけ() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let guard = "if self.skills_view && !self.skills.scanned {";
    let body = src.split(guard).nth(1).expect("Skills の走査ガードが無い");
    assert!(
        body.trim_start()
            .starts_with("self.skills.entries = skills::scan("),
        "ガードの直後で走査していない: {:?}",
        body.chars().take(60).collect::<String>()
    );
    // 到達経路 2 つ (パレット / ボトムパネルのタブ) と描画・実行部
    for route in [
        "Cmd::OpenSkills => self.open_skills_panel(),",
        "self.skills_view = !self.skills_view;",
        "skills_action = skills::ui(ui, &theme, &mut self.skills);",
        "self.apply_skills_action(skills_action, ctx);",
    ] {
        assert!(src.contains(route), "{route} が無い (画面から到達できない)");
    }
    // 「送る」は既存のエージェント送信経路をそのまま使う (経路を増やさない)
    assert!(
        src.contains("skills::SkillAction::Send(text) => self.send_to_agent(text),"),
        "送信が既存の send_to_agent を通っていない"
    );
}

/// 中央ビューの分岐は**生のフラグではなく `self.center`** を見る。
///
/// 生のフラグを見ると、描画中に押された「看板」でフラグが変わり、
/// 同じフレームに 2 つのビューが描かれて重なる。
#[test]
fn 中央ビューの分岐はスナップショットを見る() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    // フレーム冒頭で 1 回だけ畳んでいる
    let upd = src
        .split("fn update_impl(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {")
        .nth(1)
        .expect("update_impl がある");
    assert!(
        upd.contains("self.center = center_view(self.cockpit, self.kanban, self.deck);"),
        "フレーム冒頭で中央ビューを 1 つに畳んでいない"
    );
    // 中央パネルの分岐がスナップショットを見ている
    assert!(
        src.contains("if self.center == CenterView::Deck {")
            && src.contains("} else if self.center == CenterView::Kanban {")
            && src.contains("} else if self.center == CenterView::Cockpit {"),
        "中央パネルの分岐が生のフラグを見ている"
    );
    // 下部端末パネルはエディタ表示のときだけ出す。看板・デッキ・Cockpit は
    // 中央パネル全面を使う画面なので、下部パネルとは同時に描かない。
    assert!(
        src.contains("let show = self.agents.panel_open && self.center == CenterView::Editor;"),
        "端末パネルの表示判定が生のフラグを見ている"
    );
}

/// 看板は中央パネル全面で描く (下部 300px の中に押し込めない)。
#[test]
fn 看板は中央パネル全面で描く() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    // 中央パネルに看板の描画口がある
    assert!(
        src.contains("me.kanban_ui(ui, &ctx)"),
        "看板の描画口が中央パネルに無い"
    );
    // 端末パネル側には残っていない (2 か所で描くと egui の Id が衝突する)
    let term = src
        .split("fn terminal_panel(&mut self, ctx: &egui::Context) {")
        .nth(1)
        .expect("terminal_panel がある");
    let term = &term[..crate::app::method_end(term)];
    assert!(
        !term.contains("kanban_ui"),
        "看板が端末パネルの中にも残っている"
    );
}

/// 見出し行は狭いときアイコンだけに縮退する。
#[test]
fn 見出しは狭いとアイコンだけになる() {
    assert!(cockpit_header_compact(600.0));
    assert!(cockpit_header_compact(COCKPIT_HEADER_COMPACT_W - 1.0));
    assert!(!cockpit_header_compact(COCKPIT_HEADER_COMPACT_W));
    assert!(!cockpit_header_compact(1600.0));
}

/// **複数行フォームは決して見出し行へ畳み込まない。**
///
/// 畳み込むと横並びの 1 行に押し込まれ、右端の細い帯へ折り返されて
/// 見出しの下に数百 px の空白ができる (実際に起きた不具合)。
/// 1 行帯は残り幅が足りる限り畳み込む — 密度の目標は「見出しは 1 行」。
#[test]
fn 複数行フォームは見出し行へ畳み込まない() {
    for w in [
        0.0_f32,
        100.0,
        COMPOSER_INLINE_MIN_W - 1.0,
        COMPOSER_INLINE_MIN_W,
        1200.0,
    ] {
        assert!(!composer_fits_header(true, w), "w={w}: 複数行を畳み込んだ");
    }
    assert!(!composer_fits_header(false, COMPOSER_INLINE_MIN_W - 1.0));
    assert!(composer_fits_header(false, COMPOSER_INLINE_MIN_W));
    assert!(composer_fits_header(false, 1200.0));
}

/// 見出し帯の矩形: 行とコンポーザは重ならず、どちらも可用領域の中。
/// 畳み込めたときは見出し帯の高さが**行高そのまま** (密度の目標)。
#[test]
fn 見出し帯の矩形は重ならず領域内に収まる() {
    let row_h = 24.0_f32;
    let form_h = 168.0_f32;
    for avail in areas() {
        for expanded in [false, true] {
            for remaining in [0.0_f32, 120.0, 300.0, 900.0] {
                let l = cockpit_header_layout(avail, expanded, remaining, row_h, form_h);
                assert!(avail.contains_rect(l.row), "行 {:?} が領域外", l.row);
                if let Some(c) = l.composer {
                    assert!(avail.contains_rect(c), "コンポーザ {c:?} が領域外");
                    assert!(
                        c.top() >= l.row.bottom(),
                        "コンポーザが見出し行と重なっている"
                    );
                } else {
                    assert!(!expanded, "複数行フォームなのに畳み込まれた");
                    assert_eq!(
                        l.height(),
                        row_h.min(avail.height()),
                        "1 行に収まっていない"
                    );
                }
            }
        }
    }
}
