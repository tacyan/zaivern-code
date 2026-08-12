use super::*;
use crate::editor_ops::MultiSel;

/// 複数キャレットへの一括挿入は **1 回の本文差し替え**で終わる
/// = egui の取り消しスタックにも 1 段しか積まれない。
#[test]
fn 一括挿入は一度の差し替えで期待した本文になる() {
    let text = "aaa\nbbb\nccc\n";
    // 各行の先頭 (0, 4, 8) に空キャレットを立てる
    let sel = MultiSel::in_text(text, [0..0, 4..4, 8..8]);
    let (out, next, n) = multi_batch_insert(text, &sel, "// ");
    assert_eq!(out, "// aaa\n// bbb\n// ccc\n");
    assert_eq!(n, 3, "3 箇所へ入った");
    assert_eq!(next.len(), 3, "キャレットは 3 本のまま");
    // 挿入後の位置は挿入文字列の直後 (= そこにタイプした形)
    assert_eq!(next.carets()[0], 3..3);
}

/// 選択のある複数キャレットでは、選択内容を残したまま手前へ入る。
#[test]
fn 選択つきキャレットでも選択内容は消えない() {
    let text = "foo bar foo";
    let sel = MultiSel::in_text(text, [0..3, 8..11]);
    let (out, _next, n) = multi_batch_insert(text, &sel, "<");
    assert_eq!(out, "<foo bar <foo");
    assert_eq!(n, 2);
}

/// マルチカーソルの一括編集は取り消し **1 段**で丸ごと戻る。
#[test]
fn マルチカーソルの一括編集は一段で戻る() {
    use crate::editor::{Edit, HistoryLimits};
    let mut ed = Editor::new();
    ed.new_untitled();
    let mut b = ed.buffers.pop().expect("untitled タブ");
    let src = "a\nb\nc";
    b.reset_text(src.into());
    let sel = editor_ops::MultiSel::in_text(&b.text, [0..0, 2..2, 4..4]);
    let (out, next, n) = multi_batch_insert(&b.text, &sel, "// ");
    assert_eq!(n, 3, "3 箇所に入った");
    let after = next.to_single_selection_chars(&out);
    let step = Edit::programmatic(0, HistoryLimits::default())
        .with_sel_before((0, 0))
        .to_sel((after.start, after.end));
    assert!(b.apply_edit(out, step));
    assert_eq!(b.text, "// a\n// b\n// c");
    assert_eq!(b.undo(), Some((0, 0)), "1 回で編集前の位置へ戻る");
    assert_eq!(b.text, src, "3 箇所ぶんが 1 回の取り消しで戻る");
    assert!(b.redo().is_some());
    assert_eq!(b.text, "// a\n// b\n// c");
}

/// 本文への書き込みは **1 か所だけ**。
/// 途中で複数回代入すると取り消しが 1 段では戻らなくなる。
#[test]
fn 一括編集の本文書き込みは一度だけ() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("Cmd::MultiPaste => {")
        .nth(1)
        .expect("MultiPaste の腕がある");
    let arm = &body[..body.find("Cmd::ColumnSelectStart =>").unwrap_or(body.len())];
    assert_eq!(
        arm.matches("apply_edit(").count(),
        1,
        "本文を 2 回以上書き換えている (取り消しが 1 段で戻らない)"
    );
    assert!(
        !arm.contains(".text = "),
        "履歴を通さず本文へ直接代入している (取り消しに乗らない)"
    );
    assert!(
        arm.contains("multi_batch_insert("),
        "一括編集を通っていない"
    );
}

/// char 添字 → (行, 表示桁)。矩形選択の座標変換。
#[test]
fn 表示桁はタブを展開して数える() {
    let tw = 4;
    // "\tab" → タブは 0→4 桁へ、'a' が 4 桁目
    assert_eq!(char_index_to_line_col("\tab", 0, tw), (0, 0));
    assert_eq!(char_index_to_line_col("\tab", 1, tw), (0, 4));
    assert_eq!(char_index_to_line_col("\tab", 2, tw), (0, 5));
    // 改行で行が進み桁は 0 へ
    assert_eq!(char_index_to_line_col("ab\ncd", 3, tw), (1, 0));
    assert_eq!(char_index_to_line_col("ab\ncd", 5, tw), (1, 2));
    // CR は桁を進めない (CRLF の途中に桁を作らない)
    assert_eq!(char_index_to_line_col("ab\r\ncd", 4, tw), (1, 0));
    // 範囲外はクランプ (壊れた値でも落ちない)
    assert_eq!(char_index_to_line_col("ab", 99, tw), (0, 2));
}

// ──────────── 打鍵の横取り (全キャレットへ配る) ────────────

fn key_ev(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

/// 修飾キー無しの Backspace / Delete / Enter と文字入力だけを横取りする。
/// ⌘Z (取り消し) や ⌥⌫ (単語削除) を奪うと、複数キャレット中だけ
/// それらが効かなくなる。
#[test]
fn 打鍵は修飾キー無しのときだけ横取りする() {
    let none = egui::Modifiers::NONE;
    let shift = egui::Modifiers::SHIFT;
    let cmd = egui::Modifiers::COMMAND;
    let alt = egui::Modifiers::ALT;
    let cases: Vec<(egui::Event, Option<MultiKey>)> = vec![
        (
            egui::Event::Text("a".into()),
            Some(MultiKey::Text("a".into())),
        ),
        (
            egui::Event::Text("あ".into()),
            Some(MultiKey::Text("あ".into())),
        ),
        (egui::Event::Text(String::new()), None),
        (
            key_ev(egui::Key::Backspace, none),
            Some(MultiKey::Backspace),
        ),
        (
            key_ev(egui::Key::Backspace, shift),
            Some(MultiKey::Backspace),
        ),
        (key_ev(egui::Key::Backspace, alt), None),
        (key_ev(egui::Key::Backspace, cmd), None),
        (key_ev(egui::Key::Delete, none), Some(MultiKey::Delete)),
        (key_ev(egui::Key::Enter, none), Some(MultiKey::Enter)),
        (key_ev(egui::Key::Z, cmd), None),
        (key_ev(egui::Key::ArrowDown, none), None),
        (egui::Event::Copy, None),
    ];
    for (ev, want) in cases {
        assert_eq!(multi_key_of(&ev), want, "{ev:?}");
    }
    // 押下だけを拾う (離鍵で二度打たない)
    let release = egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: false,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    assert_eq!(multi_key_of(&release), None);
}

/// 抜き取った打鍵だけがイベント列から消え、残りは `TextEdit` へ流れる。
#[test]
fn 横取りした打鍵だけがイベント列から消える() {
    let mut events = vec![
        egui::Event::Text("a".into()),
        key_ev(egui::Key::ArrowRight, egui::Modifiers::NONE),
        key_ev(egui::Key::Backspace, egui::Modifiers::NONE),
        egui::Event::Text("b".into()),
    ];
    let ops = take_multi_keys(&mut events);
    assert_eq!(
        ops,
        vec![
            MultiKey::Text("a".into()),
            MultiKey::Backspace,
            MultiKey::Text("b".into()),
        ],
        "届いた順に全部返る"
    );
    assert_eq!(events.len(), 1, "矢印キーだけが残って TextEdit へ流れる");
}

/// 1 フレームに複数届いた打鍵を順に当てる (速いタイプ / キーリピート)。
/// 1 つだけ拾って残りを流すと、あふれたぶんが主キャレットにだけ入る。
#[test]
fn 一フレームの打鍵は順番どおり全キャレットへ入る() {
    let text = "x\nx\nx\n";
    let sel = MultiSel::in_text(text, [0..1, 2..3, 4..5]);
    let ops = vec![
        MultiKey::Text("a".into()),
        MultiKey::Text("b".into()),
        MultiKey::Backspace,
        MultiKey::Text("c".into()),
    ];
    let (out, next) = apply_multi_keys(text, &sel, &ops);
    assert_eq!(out, "ac\nac\nac\n");
    assert_eq!(next.len(), 3, "キャレットは 3 本のまま");
}

/// Enter は全キャレットへ入り、キャレットは新しい行へ再配置される。
#[test]
fn enter_も全キャレットへ配られる() {
    let text = "ab\ncd\n";
    let sel = MultiSel::in_text(text, [2..2, 5..5]);
    let (out, next) = apply_multi_keys(text, &sel, &[MultiKey::Enter]);
    assert_eq!(out, "ab\n\ncd\n\n");
    assert_eq!(next.len(), 2);
}

/// 空集合へ打鍵しても本文は変わらない (キャレット 0 本)。
#[test]
fn キャレットが無ければ横取りしても本文は変わらない() {
    let text = "そのまま";
    let sel = MultiSel::default();
    let (out, next) = apply_multi_keys(text, &sel, &[MultiKey::Text("x".into())]);
    assert_eq!(out, text);
    assert!(next.is_empty());
}

#[test]
fn バイト範囲からchar範囲への変換は多バイトでも合う() {
    let text = "あいうABC";
    // "あい" = 6 バイト = 2 文字
    assert_eq!(byte_range_to_char_range(text, &(0..6)), (0, 2));
    assert_eq!(byte_range_to_char_range(text, &(9..12)), (3, 6));
    // 範囲外はクランプ、文字境界でない値は手前へ寄せる (落ちない)
    assert_eq!(byte_range_to_char_range(text, &(0..999)), (0, 6));
    assert_eq!(byte_range_to_char_range(text, &(1..4)), (0, 1));
    assert_eq!(byte_range_to_char_range("", &(0..0)), (0, 0));
}

// ──────────── 追加キャレット / 選択範囲の描画 ────────────

/// 行の矩形を等間隔で作る (視覚行の並びを模す)。
fn rows(n: usize, w: f32, h: f32) -> Vec<egui::Rect> {
    (0..n)
        .map(|i| {
            egui::Rect::from_min_max(
                egui::pos2(0.0, i as f32 * h),
                egui::pos2(w, (i + 1) as f32 * h),
            )
        })
        .collect()
}

/// 1 行に収まる選択は x0..x1 の 1 枚。
#[test]
fn 一行の選択は矩形一枚になる() {
    let r = rows(1, 300.0, 16.0);
    let got = selection_row_rects(&r, 40.0, 120.0, 4.0);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].left(), 40.0);
    assert_eq!(got[0].right(), 120.0);
    assert_eq!(got[0].top(), 0.0);
    assert_eq!(got[0].bottom(), 16.0);
}

/// 行をまたぐ選択は「開始行は x から行末」「中間行は丸ごと」
/// 「終了行は行頭から x」に割れる。
#[test]
fn 行をまたぐ選択は行ごとに割れる() {
    let r = rows(3, 200.0, 16.0);
    let got = selection_row_rects(&r, 50.0, 80.0, 4.0);
    assert_eq!(got.len(), 3);
    assert_eq!((got[0].left(), got[0].right()), (50.0, 204.0));
    assert_eq!((got[1].left(), got[1].right()), (0.0, 204.0));
    assert_eq!((got[2].left(), got[2].right()), (0.0, 80.0));
}

/// どんな入力でも: 矩形は行の上下に収まり、互いに重ならず、幅は正。
/// 極端なサイズ (狭い / 広い / 逆順の x / 行が無い) で固定する。
#[test]
fn 選択矩形は行に収まり重ならない() {
    let cases: Vec<(Vec<egui::Rect>, f32, f32, f32)> = vec![
        (rows(1, 900.0, 18.0), 0.0, 900.0, 0.0),
        (rows(2, 1200.0, 12.0), 1199.0, 1.0, 6.0),
        (rows(40, 300.0, 700.0 / 40.0), 10.0, 290.0, 3.0),
        (rows(3, 120.0, 20.0), 200.0, 0.0, 5.0), // x0 が行末より右 / x1 が行頭
        (rows(0, 100.0, 10.0), 0.0, 10.0, 2.0),  // 行が無い
        (rows(2, 0.0, 10.0), 0.0, 0.0, 0.0),     // 幅 0 の行 (空行)
    ];
    for (r, x0, x1, nl) in cases {
        let got = selection_row_rects(&r, x0, x1, nl);
        let bounds = r.iter().fold(None::<egui::Rect>, |acc, x| {
            Some(acc.map_or(*x, |a| a.union(*x)))
        });
        for (i, g) in got.iter().enumerate() {
            assert!(g.width() > 0.0, "幅 0 の矩形を painter へ渡している: {g:?}");
            let b = bounds.expect("矩形があるなら行もある");
            assert!(
                g.top() >= b.top() - 0.01 && g.bottom() <= b.bottom() + 0.01,
                "行の外へはみ出した: {g:?} / {b:?}"
            );
            if i > 0 {
                assert!(
                    got[i - 1].bottom() <= g.top() + 0.01,
                    "矩形が縦に重なった: {:?} と {g:?}",
                    got[i - 1]
                );
            }
        }
    }
}

// ──────────── 配線の回帰テスト (ソース検査) ────────────

/// 打鍵の横取りは **`TextEdit` を描く前**でなければならない。
/// 後ろに置くと egui が先に主キャレットへ適用してしまい、
/// 「⌘D で 5 箇所選んだのに 1 箇所にしか入らない」に戻る。
#[test]
fn 打鍵の横取りは_textedit_より前にある() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn code_editor_ui(&mut self, ui: &mut egui::Ui) {")
        .nth(1)
        .expect("code_editor_ui がある");
    let take = body.find("take_multi_keys(").expect("打鍵の横取りがある");
    let te = body
        .find("egui::TextEdit::multiline(&mut target)")
        .expect("本文の TextEdit がある");
    assert!(
        take < te,
        "打鍵の横取りが TextEdit より後ろにある (1 箇所にしか入らない)"
    );
}

/// 複数キャレットの打鍵でも本文の書き込みは **1 か所だけ**、かつ
/// 履歴の入口 (`apply_edit`) を通ること。2 回以上書くか、履歴を迂回すると
/// 取り消しが 1 段で戻らない。
#[test]
fn 打鍵の一括適用でも本文書き込みは一度だけ() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("let ops = ui.input_mut(|i| take_multi_keys(&mut i.events));")
        .nth(1)
        .expect("横取りの腕がある");
    let arm = &body[..body.find("// 括弧・引用符の自動ペア").unwrap_or(body.len())];
    assert_eq!(
        arm.matches(".apply_edit(new_text, ed)").count(),
        1,
        "本文を 2 回以上書き換えている (取り消しが 1 段で戻らない)"
    );
    assert_eq!(
        arm.matches(".text = ").count(),
        0,
        "履歴を迂回して本文を書き換えている (apply_edit を通すこと)"
    );
    assert!(arm.contains("apply_multi_keys("), "一括適用を通っていない");
}

/// Alt+クリック / Alt+ドラッグの経路が生きていること。
/// `modifiers.alt` を読む場所が無くなったら追加キャレットは置けない。
#[test]
fn alt_クリックとドラッグの経路がある() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn code_editor_ui(&mut self, ui: &mut egui::Ui) {")
        .nth(1)
        .expect("code_editor_ui がある");
    assert!(body.contains("i.modifiers.alt"), "Alt を読んでいない");
    for want in [
        "MultiPointer::DragStart(",
        "MultiPointer::Drag(",
        "MultiPointer::Click(",
        "MultiPointer::Clear",
    ] {
        assert!(body.contains(want), "{want} の経路が無い");
    }
    assert!(
        body.contains("self.apply_multi_pointer("),
        "拾った操作を反映していない"
    );
}

/// 追加キャレットの色はテーマから取る (固定色を書かない)。
#[test]
fn 追加キャレットの色はテーマ由来() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn code_editor_ui(&mut self, ui: &mut egui::Ui) {")
        .nth(1)
        .expect("code_editor_ui がある");
    let decl = body
        .split("let multi_caret_color =")
        .nth(1)
        .expect("キャレット色の宣言がある");
    let decl = &decl[..decl.find(';').unwrap_or(decl.len())];
    assert!(decl.contains("self.theme."), "テーマ由来でない色: {decl}");
}
