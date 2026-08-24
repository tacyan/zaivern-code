//! ファイル内検索 (⌘F) の**フォーカスの居場所**を固定する番人。
//!
//! 直った不具合はこれ: 検索欄に 1 文字打つと、その打鍵が**検索語と本文の
//! 両方**へ入っていた。
//!
//! * 検索バーは本文より**先**に描かれる (`editor_area`)。
//! * 打鍵ごとにインクリメンタル検索が走り、ヒットへキャレットを移す
//!   (`find_step` → `pending_select`)。
//! * その適用が `request_focus` まで**無条件に**やっていたので、同じフレームの
//!   あとに描かれる本文の `TextEdit` が「フォーカスあり」になり、
//!   **まだ入力キューに残っている同じ打鍵**を自分にも適用していた
//!   (egui は `TextEdit` が処理したイベントをキューから消さない)。
//! * キャレットはヒットを**選択**した状態にしてあるので、打った 1 文字は
//!   ヒットした文字列を**置き換える**。2 文字目からは検索欄ではなくファイルへ入る。
//!
//! VS Code は検索中フォーカスを検索欄に置いたままヒットだけを動かす。
//! 同じにするため、フォーカス移動は `apply_pending_select` の `focus` 引数に
//! 閉じ、検索経路だけ false を渡す。
//!
//! その**二次被害**もここで見張る: 本文が同じフレームで変わると、検索ヒットは
//! 1 フレーム古い本文の位置を指す。ズレた位置で CJK の途中を切ると epaint が
//! 落ちる (利用者の `panic.log` に 7 回残っていた)。塗るのは「ヒットを数えた
//! 本文」と「いま組む文字列」が一致するときだけにする。

use super::*;

/// 3 フレーム分の入力を流す小さなハーネス。実際の並び (検索バー → 本文) を
/// そのまま再現する。`steal` が本番の分岐 (フォーカスも移すか)。
fn type_into_find(steal: bool) -> (String, String) {
    let ctx = egui::Context::default();
    let find_id = egui::Id::new("t-find-query");
    let body_id = egui::Id::new("t-editor-body");
    let mut query = String::new();
    let mut body = String::from("hello\n");
    let mut focus_find = true;

    let mut frame = |events: Vec<egui::Event>, query: &mut String, body: &mut String| {
        let input = egui::RawInput {
            events,
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                // ── 検索バー (本文より先に描かれる) ──
                let resp = ui.add(egui::TextEdit::singleline(query).id(find_id));
                if focus_find {
                    resp.request_focus();
                    focus_find = false;
                }
                let typed = resp.changed();
                // ── 本文 (find_step が予約したキャレット移動をここで適用する) ──
                if typed {
                    // ヒットを選択した状態にする = 実際の `find_step` と同じ形
                    apply_pending_select(ui.ctx(), body_id, (0, 1), steal);
                }
                ui.add(egui::TextEdit::multiline(body).id(body_id));
            });
        });
    };

    frame(Vec::new(), &mut query, &mut body); // 1: 検索欄へフォーカス
    frame(vec![egui::Event::Text("i".into())], &mut query, &mut body); // 2: 1 文字目
    frame(vec![egui::Event::Text("n".into())], &mut query, &mut body); // 3: 2 文字目
    (query, body)
}

/// **不具合の再現**: フォーカスまで本文へ移すと、1 文字目でファイルが書き換わり、
/// 2 文字目からは検索語に入らなくなる。
#[test]
fn 本文がフォーカスを奪うと打鍵がファイルへ入る() {
    let (query, body) = type_into_find(true);
    assert_eq!(
        query, "i",
        "2 文字目が検索語に入らない (フォーカスを失った)"
    );
    assert_ne!(
        body, "hello\n",
        "検索欄へ打っただけなのに本文が書き換わった"
    );
}

/// **直した後**: 検索中はフォーカスが検索欄に残り、本文は 1 バイトも変わらない。
#[test]
fn 検索欄で打っている間は本文が変わらない() {
    let (query, body) = type_into_find(false);
    assert_eq!(query, "in", "打った文字はそのまま検索語になる");
    assert_eq!(body, "hello\n", "本文は 1 バイトも変わらない");
}

/// `apply_pending_select` は `focus` が false でも**キャレットは動かす**
/// (ヒットの選択は要る。要らないのはフォーカスだけ)。
#[test]
fn フォーカスを移さなくてもキャレットは動く() {
    for focus in [false, true] {
        let ctx = egui::Context::default();
        let id = egui::Id::new("t-body-only");
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                apply_pending_select(ui.ctx(), id, (2, 5), focus);
            });
        });
        let r = egui::TextEdit::load_state(&ctx, id)
            .and_then(|st| st.cursor.char_range())
            .expect("キャレットは focus に関係なく置かれる");
        // `CCursorRange::two(min, max)` は max 側が primary (= キャレットは後ろ、
        // 錨は前)。並びは egui の約束なので、範囲そのもので照合する。
        let (lo, hi) = (
            r.primary.index.min(r.secondary.index),
            r.primary.index.max(r.secondary.index),
        );
        assert_eq!((lo, hi), (2, 5), "ヒットを選択した状態になる");
        assert_eq!(
            ctx.memory(|m| m.focused()) == Some(id),
            focus,
            "focus={focus} のときのフォーカス要求が違う"
        );
    }
}

/// インクリメンタル検索の経路が**フォーカスを渡さない**ことを、
/// `find_step` の**関数の中だけ**を見て固定する。
#[test]
fn find_stepはフォーカスを本文へ渡さない() {
    let src = SRC_IMPL.replace("\r\n", "\n");
    let body = src
        .split("pub(super) fn find_step")
        .nth(1)
        .expect("find_step がある");
    let body = &body[..method_end(body)];
    assert!(
        body.contains("self.pending_select_focus = false;"),
        "find_step がフォーカスを本文へ渡している (打鍵の 2 文字目からファイルへ入る)"
    );
}

/// 本文を描く側は `request_focus` を直接呼ばない。
/// キャレット移動に伴うフォーカスは `apply_pending_select` の 1 か所に閉じる
/// (2 か所目ができると、片方だけ直して直った気になる)。
#[test]
fn 本文の描画はフォーカスを直接要求しない() {
    let src = include_str!("code_editor.rs").replace("\r\n", "\n");
    let hits: Vec<&str> = src
        .lines()
        .filter(|l| l.contains("request_focus") && !l.trim_start().starts_with("//"))
        .collect();
    assert!(
        hits.is_empty(),
        "code_editor.rs が直接フォーカスを要求している: {hits:?}"
    );
}

/// 検索バーは Esc で閉じられる (VS Code と同じ)。フォーカスを奪わなくなった
/// ぶん、**キーボードだけで本文へ戻る道**が要る。
#[test]
fn 検索バーはescで閉じられる() {
    let src = SRC_IMPL.replace("\r\n", "\n");
    let body = src
        .split("pub(super) fn find_bar")
        .nth(1)
        .expect("find_bar がある");
    let body = &body[..method_end(body)];
    let code: String = body
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        code.contains("egui::Key::Escape"),
        "Esc で検索バーを閉じる経路が無い"
    );
}

/// 検索ヒットの塗りは、**ヒットを数えた本文と一致するときだけ**当てる。
///
/// 無条件に当てると (1) 折りたたみ表示のような別の文字列へ当ててしまい
/// (2) 本文が同じフレームで変わったときは CJK の途中を切って epaint が落ちる。
/// 鍵に混ぜていないと、塗らずに組んだ galley が次のフレームも再利用されて
/// ハイライトが永久に出なくなる — **両方**を見る。
#[test]
fn 検索ハイライトは同じ本文にしか当てない() {
    let src = include_str!("code_editor.rs").replace("\r\n", "\n");
    let at = src
        .find("find_buffer::apply_hits(")
        .expect("apply_hits の呼び出しがある");
    // 遡る先は**文字境界へ寄せる** (このファイルは日本語のコメントだらけで、
    // 素朴に引き算すると自分がいま直しているのと同じ形で落ちる)。
    let mut from = at.saturating_sub(300);
    while from < at && !src.is_char_boundary(from) {
        from += 1;
    }
    let before = &src[from..at];
    assert!(
        before.contains("hits_fit"),
        "apply_hits を無条件に呼んでいる (別の本文へ当たると epaint が落ちる)"
    );
    let code: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        code.contains("find_text_hash,"),
        "galley の鍵に find_text_hash が入っていない (塗らずに組んだ galley を使い回す)"
    );
}
