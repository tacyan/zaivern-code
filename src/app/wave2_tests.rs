use super::*;
use crate::editor::{Bookmarks, ClosedTab, ClosedTabs, FoldState};

// ── B: 折りたたみ ────────────────────────────────────────────

/// ガターの ▸/▾ を押すと、その行の折りたたみ状態が反転する。
/// (押した位置 → 行の解決 → FoldState の更新、までを通しで見る)
#[test]
fn ガターのクリックで折りたたみが開閉する() {
    let text = "fn a() {\n    let x = 1;\n    let y = 2;\n}\nfn b() {}\n";
    let mut folds = FoldState::default();
    assert!(folds.refresh(text, "Rust"), "初回は必ず計算する");
    assert!(folds.is_foldable(0), "1 行目は畳める");

    // 行 0 が y 10..20、行 1 が y 20..30 に描かれているとする
    let rows = vec![(0usize, 10.0_f32, 20.0_f32), (1usize, 20.0_f32, 30.0_f32)];
    let marks: HashMap<usize, bool> = folds
        .ranges()
        .iter()
        .map(|r| (r.start_line, folds.is_folded(r.start_line)))
        .collect();
    let fold_x = 40.0_f32;

    // ▸ の桁より左 (ブックマークの列) は反応しない
    assert_eq!(
        fold_click_line(&rows, &marks, fold_x, egui::pos2(30.0, 15.0)),
        None
    );
    // 畳めない行を押しても反応しない
    assert_eq!(
        fold_click_line(&rows, &marks, fold_x, egui::pos2(45.0, 25.0)),
        None
    );
    // 畳める行の ▸ を押すとその行が返る
    let hit = fold_click_line(&rows, &marks, fold_x, egui::pos2(45.0, 15.0));
    assert_eq!(hit, Some(0));

    assert!(!folds.is_folded(0));
    assert!(folds.toggle_fold(hit.expect("行が取れる")));
    assert!(folds.is_folded(0), "クリックで畳まれる");
    assert!(folds.toggle_fold(0));
    assert!(!folds.is_folded(0), "もう一度押すと開く");
}

/// 畳んだ範囲の行は表示テキストに現れず、キャレット添字が原文へ正しく写る。
#[test]
fn 畳んだ行は描かれずキャレットが原文へ写る() {
    let src = "a\nHIDE1\nHIDE2\nb\n";
    // 行 1..=2 を隠す (行 0 がヘッダ)
    let (disp, lines, cut) = build_fold_view(src, &[(1, 2)]);
    assert_eq!(disp, "a\nb\n", "隠した行は 1 文字も残さない");
    assert!(!disp.contains("HIDE"));
    // 表示行 → 原文行
    assert_eq!(lines, vec![0, 3, 4]);

    // 表示の "b" (添字 2) は原文の "b" (添字 14) を指す
    let src_b = src.chars().position(|c| c == 'b').expect("b がある");
    assert_eq!(fold_display_to_source(&cut, 2), src_b);
    // 逆写像も一致する
    assert_eq!(fold_source_to_display(&cut, src_b), 2);
    // 隠れている位置は折りたたみの直前へ丸める (畳んだ中に入らない)
    let hidden_idx = src.find("HIDE2").expect("ある");
    assert_eq!(fold_source_to_display(&cut, hidden_idx), 2);

    // 行頭は写像の両側で一致する (キャレットが行をまたいでもズレない)
    for d in 0..=disp.chars().count() {
        let s = fold_display_to_source(&cut, d);
        assert!(s <= src.chars().count());
    }
}

/// 表示テキストへの編集は、隠した行を保ったまま原文へ差し戻る。
#[test]
fn 折りたたみ中の編集が原文へ差し戻る() {
    let src = "a\nHIDE1\nHIDE2\nb\n";
    let (disp, _, cut) = build_fold_view(src, &[(1, 2)]);
    assert_eq!(disp, "a\nb\n");

    // 可視行 "b" の後ろに "X" を打つ
    let edited = "a\nbX\n";
    let next = splice_fold_edit(src, &cut, &disp, edited);
    assert_eq!(next, "a\nHIDE1\nHIDE2\nbX\n", "隠した行は消えない");
    let (at, delta) = fold_edit_shift(src, &next, &cut, &disp, edited);
    assert_eq!(delta, 0, "行数は変わっていない");
    assert_eq!(at, 3, "編集は原文 4 行目 (0 始まりで 3)");

    // 行を増やす編集では delta が正になる (畳んだ状態を追随させるため)
    let edited2 = "a\nb\n\n";
    let next2 = splice_fold_edit(src, &cut, &disp, edited2);
    let (_, delta2) = fold_edit_shift(src, &next2, &cut, &disp, edited2);
    assert_eq!(delta2, 1);

    // 何も変えなければ原文はそのまま (無編集フレームで壊さない)
    assert_eq!(splice_fold_edit(src, &cut, &disp, &disp), src);
}

/// 末尾まで畳むときは手前の改行ごと落とし、空行を残さない。
#[test]
fn 末尾まで畳んでも空行が残らない() {
    let src = "head\nx\ny";
    let (disp, lines, cut) = build_fold_view(src, &[(1, 2)]);
    assert_eq!(disp, "head");
    assert_eq!(lines, vec![0]);
    assert_eq!(cut.len(), 1);
    // 差し戻しても原文が保たれる
    assert_eq!(splice_fold_edit(src, &cut, &disp, &disp), src);
}

/// 折りたたみが無いときは表示テキスト = 原文 (恒等)。
#[test]
fn 折りたたみ無しでは表示テキストが原文と同じ() {
    let src = "a\nb\nc\n";
    let (disp, lines, cut) = build_fold_view(src, &[]);
    assert_eq!(disp, src);
    assert_eq!(lines, vec![0, 1, 2, 3]);
    assert!(cut.is_empty());
    for d in 0..=src.chars().count() {
        assert_eq!(fold_display_to_source(&cut, d), d);
        assert_eq!(fold_source_to_display(&cut, d), d);
    }
}

/// 折り返し ON でも、行番号・印は「表示行の先頭の視覚行」だけに出る。
#[test]
fn 視覚行から表示行の先頭だけを拾う() {
    // 行 0 が 2 行に折り返し、行 1 は折り返し無し、行 2 が 3 行に折り返し
    let nl = [false, true, true, false, false, true];
    let got = row_line_starts(&nl);
    assert_eq!(got, vec![(0, 0), (2, 1), (3, 2)]);
    // 折り返し無しなら恒等
    let flat = [true, true, true];
    assert_eq!(row_line_starts(&flat), vec![(0, 0), (1, 1), (2, 2)]);
}

// ── B: インデントガイド / スティッキー ────────────────────────

/// インデントガイドと強調ガイドが、描画側が使う形で出てくる。
#[test]
fn インデントガイドと強調ガイドが描画へ流れる() {
    let text = "fn a() {\n    if x {\n        y();\n    }\n}\n";
    let tw = crate::highlight::DEFAULT_TAB_WIDTH;
    let guides = crate::highlight::indent_guides(text, tw);
    // 契約: 行数ぶんの要素があり、v[i].0 == i
    assert_eq!(guides.len(), text.split('\n').count());
    for (i, (n, _)) in guides.iter().enumerate() {
        assert_eq!(*n, i);
    }
    // 一番深い行 (y(); = 行 2) には 2 本の縦線が要る
    assert_eq!(guides[2].1, vec![0, tw]);
    // 一番外側の行には縦線が無い
    assert!(guides[0].1.is_empty());

    // 強調ガイド: キャレットが if の中にいるとき、その桁が返る
    let ag = crate::highlight::active_guide(text, tw, 2).expect("囲むブロックがある");
    assert_eq!(ag.column, tw);
    assert!(ag.start_line <= 2 && 2 <= ag.end_line);
    // 描画側の判定 (この行のこの桁を強調するか) と噛み合う
    let on = guides[2].1.contains(&ag.column) && ag.start_line <= 2 && 2 <= ag.end_line;
    assert!(on, "強調する桁がガイドの桁集合に含まれている");
}

/// スティッキーヘッダが「上端に貼る行」を外側から順に返す。
#[test]
fn スティッキーヘッダが上端の文脈を返す() {
    let text = "fn outer() {\n    if x {\n        a();\n        b();\n    }\n}\n";
    // 3 行目 (a();) が最上部に見えているとき
    let heads = crate::highlight::sticky_headers(text, "Rust", 2, STICKY_MAX_ROWS);
    assert!(!heads.is_empty(), "囲んでいるブロックのヘッダが出る");
    assert!(heads.len() <= STICKY_MAX_ROWS);
    // 外側から順 (行番号が昇順)
    let mut prev = 0usize;
    for (n, _) in &heads {
        assert!(*n >= prev);
        prev = *n;
    }
    assert_eq!(heads[0].0, 0, "一番外側は fn outer の行");
    // 先頭が見えているときは何も貼らない
    assert!(crate::highlight::sticky_headers(text, "Rust", 0, STICKY_MAX_ROWS).is_empty());
}

// ── C: ブックマーク / 閉じたタブ ─────────────────────────────

/// ブックマークのコマンドがそれぞれの効果になる。
#[test]
fn ブックマークのコマンドが状態とジャンプ先に効く() {
    let mut m = Bookmarks::default();
    // 切替: その場から動かず、印だけ付く
    assert_eq!(bookmark_cmd_target(&Cmd::ToggleBookmark, &mut m, 3), None);
    assert!(m.is_marked(3));
    assert_eq!(bookmark_cmd_target(&Cmd::ToggleBookmark, &mut m, 10), None);
    assert!(m.is_marked(10));

    // 次 / 前: 端では折り返す
    assert_eq!(bookmark_cmd_target(&Cmd::NextBookmark, &mut m, 3), Some(10));
    assert_eq!(bookmark_cmd_target(&Cmd::NextBookmark, &mut m, 10), Some(3));
    assert_eq!(bookmark_cmd_target(&Cmd::PrevBookmark, &mut m, 10), Some(3));
    assert_eq!(bookmark_cmd_target(&Cmd::PrevBookmark, &mut m, 3), Some(10));

    // もう一度切替で外れる
    assert_eq!(bookmark_cmd_target(&Cmd::ToggleBookmark, &mut m, 3), None);
    assert!(!m.is_marked(3));

    // 全解除
    assert_eq!(bookmark_cmd_target(&Cmd::ClearBookmarks, &mut m, 0), None);
    assert!(m.is_empty());
    // 空なら次 / 前は行き先なし (呼び出し側が「ありません」を出す)
    assert_eq!(bookmark_cmd_target(&Cmd::NextBookmark, &mut m, 0), None);

    // 関係ないコマンドは何もしない
    assert_eq!(bookmark_cmd_target(&Cmd::Save, &mut m, 0), None);
    assert!(m.is_empty());
}

/// 「閉じたタブを開き直す」は最後に閉じたものから順に返る。
#[test]
fn 閉じたタブを新しい順に開き直せる() {
    let mut ct = ClosedTabs::default();
    assert!(
        ct.pop_closed().is_none(),
        "何も閉じていなければ何も返らない"
    );
    for (n, name) in ["a.rs", "b.rs"].iter().enumerate() {
        ct.push_closed(ClosedTab {
            path: PathBuf::from(name),
            title: (*name).into(),
            cursor: (n + 5, 2),
            scroll: 12.0,
        });
    }
    let t = ct.pop_closed().expect("直前に閉じたもの");
    assert_eq!(t.path, PathBuf::from("b.rs"));
    // 復元に使う値がそのまま乗っている (goto_line は 1 始まり)
    assert_eq!(t.cursor, (6, 2));
    assert!(t.scroll > 0.0);
    assert_eq!(ct.pop_closed().map(|t| t.path), Some(PathBuf::from("a.rs")));
    assert!(ct.pop_closed().is_none());
}

// ── D: テーブル表示 / 巨大ファイル ───────────────────────────

/// テーブル表示の切替判定。
#[test]
fn テーブル表示の切替判定() {
    let table: [(bool, bool, TableToggle); 4] = [
        // (CSV/TSV か, 表を出しているか, 期待)
        (true, false, TableToggle::Build),
        (true, true, TableToggle::Drop),
        // 拡張子が違っても、出しているものは必ず解除できる
        (false, true, TableToggle::Drop),
        (false, false, TableToggle::NotTable),
    ];
    for (is_table, showing, want) in table {
        assert_eq!(
            table_toggle_decision(is_table, showing),
            want,
            "is_table={is_table} showing={showing}"
        );
    }
    // 判定に使う拡張子は editor.rs の表と揃っている
    assert!(crate::editor::is_table_path(Path::new("a/b.csv")));
    assert!(crate::editor::is_table_path(Path::new("a/b.TSV")));
    assert!(!crate::editor::is_table_path(Path::new("a/b.rs")));
}

/// 表のパース結果が、グリッド描画が期待する形になっている。
#[test]
fn 表のパース結果がグリッドの形になる() {
    let t = crate::editor::parse_table("a,b,c\n1,2\n3,4,5,6\n", 100);
    assert_eq!(t.headers, vec!["a", "b", "c"]);
    assert_eq!(t.rows.len(), 2);
    // ラグド: 列数は最大値で、足りないセルは描画側が空欄で埋める
    assert_eq!(t.columns, 4);
    assert!(!t.truncated);
    assert!(t.rows[0].get(2).is_none(), "短い行は短いまま返る");

    // 上限を超えたら truncated で知らせる (画面に注意書きを出す)
    let many: String = (0..10).map(|n| format!("{n},x\n")).collect();
    let cut = crate::editor::parse_table(&many, 3);
    assert!(cut.truncated);
    assert_eq!(cut.rows.len(), 3);
}

/// 巨大ファイルの旗が、そのまま画面の状態になる。
#[test]
fn 巨大ファイルの旗が画面の状態になる() {
    use crate::editor::{open_decision, OpenDecision, HEAVY_FILE_BYTES, LARGE_FILE_BYTES};

    // 普通の大きさ: 制限なし = 帯も出ない
    let OpenDecision::Open(small) = open_decision(1024) else {
        panic!("開けるはず");
    };
    assert!(!small.active && !small.read_only && small.highlight);
    assert!(large_file_reasons(small.read_only, !small.highlight).is_empty());

    // 重いファイル: 強調表示だけ止まる (編集はできる)
    let OpenDecision::Open(heavy) = open_decision(HEAVY_FILE_BYTES + 1) else {
        panic!("開けるはず");
    };
    assert!(heavy.active && !heavy.read_only && !heavy.highlight);
    let why = large_file_reasons(heavy.read_only, !heavy.highlight);
    assert_eq!(why, vec![tr("強調表示と折りたたみを停止")]);

    // 巨大ファイル: 読み取り専用にもなる
    let OpenDecision::Open(big) = open_decision(LARGE_FILE_BYTES + 1) else {
        panic!("開けるはず");
    };
    assert!(big.active && big.read_only && !big.highlight);
    assert_eq!(
        large_file_reasons(big.read_only, !big.highlight),
        vec![tr("読み取り専用"), tr("強調表示と折りたたみを停止")]
    );
}

/// 読み取り専用の包みは編集を捨て、選択のための本文は返す。
#[test]
fn 読み取り専用の包みは編集を受け付けない() {
    use egui::TextBuffer;
    let src = String::from("abc");
    let mut ro = EditTarget::Ro(&src);
    assert!(!ro.is_mutable());
    assert_eq!(ro.insert_text("X", 0), 0);
    ro.delete_char_range(0..1);
    ro.set("zzz".into());
    assert_eq!(ro.as_str(), "abc", "読み取り専用は 1 文字も変わらない");

    let mut rw_src = String::from("abc");
    let mut rw = EditTarget::Rw(&mut rw_src);
    assert!(rw.is_mutable());
    rw.insert_text("X", 0);
    assert_eq!(rw.as_str(), "Xabc");
    rw.set("zzz".into());
    assert_eq!(rw.as_str(), "zzz");
}

// ── E: LSP ───────────────────────────────────────────────────

fn item(label: &str, insert: &str, kind: u8) -> lsp::CompletionItem {
    lsp::CompletionItem {
        label: label.into(),
        insert_text: insert.into(),
        detail: String::new(),
        documentation: String::new(),
        kind,
        text_edit: None,
        additional_text_edits: Vec::new(),
        sort_text: None,
        filter_text: None,
        preselect: false,
        is_snippet: false,
        deprecated: false,
    }
}

/// 補完の確定が本文へ当たる (サーバーは立てず、応答を合成する)。
#[test]
fn 補完の確定が本文と追加編集を当てる() {
    let mut st = lsp::CompletionState::new();
    let now = Instant::now();
    st.invoke("pri", now);
    let anchor = lsp::Position::new(2, 3);
    st.mark_sent(lsp::RequestStatus::Sent(1), anchor);

    // textEdit と additionalTextEdits (import 追加) 付きの候補を合成する
    let mut main = item("println!", "println!", 3);
    main.text_edit = Some(lsp::TextEdit::new(
        lsp::Range::new(lsp::Position::new(2, 0), lsp::Position::new(2, 3)),
        "println!",
    ));
    main.additional_text_edits = vec![lsp::TextEdit::new(
        lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 0)),
        "use std::fmt;\n",
    )];
    assert!(st.apply_response(
        1,
        lsp::CompletionList {
            is_incomplete: false,
            items: vec![main, item("print", "print", 3)],
        }
    ));
    assert!(st.is_open());
    assert_eq!(st.len(), 2, "どちらの候補も pri で絞り込みを通る");
    // 並び順はサーバー / sortText 次第なので仮定しない。矢印で狙った候補まで進む。
    for _ in 0..st.len() {
        if st.selected().map(|i| i.label.as_str()) == Some("println!") {
            break;
        }
        st.select_next();
    }
    assert_eq!(
        st.selected().map(|i| i.label.clone()),
        Some("println!".into())
    );

    let fallback = lsp::Range::new(lsp::Position::new(2, 0), anchor);
    let edits = st.accept(fallback).expect("確定できる");
    assert_eq!(edits.len(), 2, "追加編集も一緒に返る");

    let before = "mod m;\n\npri\n";
    let after = lsp::apply_text_edits(before, &edits);
    assert_eq!(after, "use std::fmt;\nmod m;\n\nprintln!\n");

    // 矢印で選び直すと当てる編集も変わる
    st.select_next();
    assert_eq!(st.selected().map(|i| i.label.clone()), Some("print".into()));
    assert!(
        st.selected()
            .map(|i| i.text_edit.is_none())
            .unwrap_or(false),
        "こちらは textEdit を持たない候補"
    );
    let edits2 = st.accept(fallback).expect("確定できる");
    assert_eq!(
        lsp::apply_text_edits(before, &edits2),
        "mod m;\n\nprint\n",
        "textEdit の無い候補は既定の範囲に差し込む"
    );

    // Esc 相当で閉じたら何も確定しない
    st.dismiss();
    assert!(!st.is_open());
    assert!(st.accept(fallback).is_none());
}

/// 補完の kind は必ずフォント非依存のラベルになる (豆腐にしない)。
#[test]
fn 補完とシンボルの種別ラベルは英字だけ() {
    for k in 0u8..=30 {
        for s in [completion_kind_label(k), symbol_kind_label(k)] {
            assert!(!s.is_empty());
            assert!(
                s.chars().all(|c| c.is_ascii_alphanumeric() || c == '·'),
                "kind={k} label={s}"
            );
        }
    }
}

/// 参照の一覧がファイルごとにまとまる (パネルの並びと同じ形)。
#[test]
fn 参照の一覧がファイルごとにまとまる() {
    let r = |l: usize| lsp::Range::new(lsp::Position::new(l, 0), lsp::Position::new(l, 4));
    let locs = vec![
        lsp::Location {
            path: PathBuf::from("/w/b.rs"),
            range: r(9),
        },
        lsp::Location {
            path: PathBuf::from("/w/a.rs"),
            range: r(3),
        },
        lsp::Location {
            path: PathBuf::from("/w/a.rs"),
            range: r(1),
        },
    ];
    let groups = lsp::group_locations(locs);
    assert_eq!(groups.len(), 2);
    let total: usize = groups.iter().map(|g| g.locations.len()).sum();
    assert_eq!(total, 3);
    let a = groups
        .iter()
        .find(|g| g.path == PathBuf::from("/w/a.rs"))
        .expect("a.rs の組がある");
    assert_eq!(a.locations.len(), 2);
    // 空の応答は空の一覧 (呼び出し側が「見つかりません」を出す)
    assert!(lsp::group_locations(Vec::new()).is_empty());
}

/// シンボル木が quick-open 用の平らな並びになる (深さつき・先行順)。
#[test]
fn シンボル一覧が深さつきで平らになる() {
    let node =
        |name: &str, kind: u8, line: usize, children: Vec<lsp::SymbolNode>| lsp::SymbolNode {
            name: name.into(),
            detail: String::new(),
            kind,
            range: lsp::Range::new(lsp::Position::new(line, 0), lsp::Position::new(line + 5, 0)),
            selection_range: lsp::Range::new(
                lsp::Position::new(line, 3),
                lsp::Position::new(line, 8),
            ),
            deprecated: false,
            children,
        };
    let tree = vec![
        node(
            "Foo",
            23,
            10,
            vec![node("bar", 6, 12, vec![]), node("baz", 6, 14, vec![])],
        ),
        node("top", 12, 30, vec![]),
    ];
    let mut flat = Vec::new();
    flatten_symbols(&tree, 0, &mut flat);
    assert_eq!(flat.len(), 4);
    let names: Vec<&str> = flat.iter().map(|(_, n, ..)| n.as_str()).collect();
    assert_eq!(
        names,
        vec!["Foo", "bar", "baz", "top"],
        "先行順で平らになる"
    );
    assert_eq!(
        flat.iter().map(|(d, ..)| *d).collect::<Vec<_>>(),
        vec![0, 1, 1, 0]
    );
    // ジャンプ先は selection_range の先頭 (名前の位置)
    assert_eq!(flat[1].3, lsp::Position::new(12, 3));
    assert_eq!(symbol_kind_label(flat[0].2), "struct");
    assert!(flatten_symbols_is_empty_for_empty_tree());
}

fn flatten_symbols_is_empty_for_empty_tree() -> bool {
    let mut v = Vec::new();
    flatten_symbols(&[], 0, &mut v);
    v.is_empty()
}

/// リネームの WorkspaceEdit が 1 ファイルぶんずつ当たる。
#[test]
fn リネームの編集がファイルごとに当たる() {
    let fe = lsp::FileEdits {
        path: PathBuf::from("/w/a.rs"),
        // 契約: 降順に並んでいる (後ろから当てる)
        edits: vec![
            lsp::TextEdit::new(
                lsp::Range::new(lsp::Position::new(2, 4), lsp::Position::new(2, 7)),
                "neu",
            ),
            lsp::TextEdit::new(
                lsp::Range::new(lsp::Position::new(0, 3), lsp::Position::new(0, 6)),
                "neu",
            ),
        ],
    };
    let before = "fn old() {}\n\n    old();\n";
    assert_eq!(
        lsp::apply_file_edits(before, &fe),
        "fn neu() {}\n\n    neu();\n"
    );
    let plan = lsp::WorkspaceEditPlan {
        files: vec![fe],
        has_resource_ops: false,
    };
    assert!(!plan.is_empty());
    assert_eq!(plan.edit_count(), 2);
    // 空の計画は「変更はありませんでした」の側へ落ちる
    assert!(lsp::WorkspaceEditPlan {
        files: Vec::new(),
        has_resource_ops: false,
    }
    .is_empty());
}

/// 整形の結果を本文へ当てる経路 (poll_formatting → apply_text_edits)。
#[test]
fn 整形の結果が本文へ当たる() {
    let edits = vec![lsp::TextEdit::new(
        lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 5)),
        "fn ",
    )];
    assert_eq!(
        lsp::apply_text_edits("fn   a() {}\n", &edits),
        "fn a() {}\n"
    );
    // 空の応答は「整形の必要はありませんでした」= 本文を触らない
    let none: Vec<lsp::TextEdit> = Vec::new();
    assert_eq!(lsp::apply_text_edits("x\n", &none), "x\n");
}

/// `sweep_timeouts` を毎フレーム呼んでいること、そのフレーム経路が
/// `update` に繋がっていることをソースで固定する。
///
/// LSP クライアントは実プロセスを起こすためテストから触れない。ここが
/// 抜けると「返らないリクエストの席が埋まったまま、以後の要求が黙って
/// 効かなくなる」という**気付けない壊れ方**をするので、配線そのものを守る。
#[test]
fn sweep_timeouts_を毎フレーム呼んでいる() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let poll = src
        .split("fn poll_lsp(&mut self) {")
        .nth(1)
        .expect("poll_lsp がある");
    let loop_body = poll
        .split("for c in self.lsp.values() {")
        .nth(1)
        .expect("全クライアントを回している");
    assert!(
        loop_body
            .split("}\n")
            .next()
            .expect("ループ本体")
            .contains("c.sweep_timeouts(lsp::REQUEST_TIMEOUT);"),
        "sweep_timeouts はクライアントごとに毎フレーム呼ぶ"
    );
    // 応答の取りこぼしが無いこと (全 poll_* を回している)
    for m in [
        "poll_completion()",
        "poll_hover()",
        "poll_references()",
        "poll_document_symbols()",
        "poll_prepare_rename()",
        "poll_rename()",
        "poll_formatting()",
    ] {
        assert!(poll.contains(m), "{m} を回収していない");
    }
    // update から毎フレーム呼ばれている
    let update = src
        .split("fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {")
        .nth(1)
        .expect("update がある");
    assert!(
        update.contains("self.poll_lsp();"),
        "update から呼んでいない"
    );
    assert!(
        update.contains("self.lsp_completion_tick(ctx);"),
        "補完のデバウンスが update から呼ばれていない"
    );
}

/// 折りたたみ → 編集 → 差し戻し → 畳んだ状態の追随、を一周させる。
///
/// 「畳んだまま上の行を編集したら、下の折りたたみが 1 行ずれる」という
/// 一番壊れやすい経路を固定する。
#[test]
fn 畳んだまま編集しても折りたたみが追随する() {
    let mut text = String::from("fn a() {\n    x();\n}\nfn b() {\n    y();\n    z();\n}\n");
    let mut folds = FoldState::default();
    folds.refresh(&text, "Rust");
    // 2 つ目の関数 (行 3 から) を畳む
    assert!(folds.toggle_fold(3), "行 3 は畳める");
    let hidden = folds.hidden_spans();
    assert_eq!(hidden, vec![(4, 6)]);

    let (disp, lines, cut) = build_fold_view(&text, &hidden);
    assert!(!disp.contains("y();") && !disp.contains("z();"));
    assert!(disp.contains("fn b() {"), "ヘッダ行は残る");
    assert_eq!(lines, vec![0, 1, 2, 3, 7]);

    // 1 つ目の関数の中に 1 行足す (畳んだ範囲より上)
    let edited = disp.replacen("    x();\n", "    x();\n    w();\n", 1);
    let next = splice_fold_edit(&text, &cut, &disp, &edited);
    assert!(next.contains("    w();"), "編集が原文へ入る");
    assert!(next.contains("    y();"), "畳んだ中身は消えない");

    let (at, delta) = fold_edit_shift(&text, &next, &cut, &disp, &edited);
    assert_eq!(delta, 1);
    folds.shift_lines(at, delta);
    text = next;
    folds.refresh(&text, "Rust");
    // 畳んだ関数は 1 行下がっても畳まれたまま
    assert!(folds.is_folded(4), "畳んだ状態が編集を跨いで残る");
    assert_eq!(folds.hidden_spans(), vec![(5, 7)]);
}

/// 折りたたみ中は、原文の添字を前提にする補助機能を止めている。
///
/// これを外すと、表示テキストのキャレット添字で原文を書き換えて
/// **本文が壊れる**。ソースで固定しておく。
#[test]
fn 折りたたみ中は添字前提の補助機能を止めている() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    assert!(
        src.contains("let folds_closed = !self.editor.buffers[active].folds.folded().is_empty();"),
        "折りたたみ中かの判定が無い"
    );
    assert!(
        src.contains("let expand = if has_focus && !folds_closed {"),
        "スニペット展開を止めていない"
    );
    assert!(
        src.contains("if has_focus && !folds_closed && !self.editor.buffers[active].read_only() {"),
        "自動ペアを止めていない / 読み取り専用を見ていない"
    );
}

/// レビュー画面が返す 2 つの追加要求を app 側で受けていること。
#[test]
fn レビュー画面の要求を受けている() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    assert!(
        src.contains("if let Some(file) = git_actions.open_file {"),
        "open_file をエディタで開いていない"
    );
    assert!(
        src.contains("if let Some(prompt) = git_actions.review_prompt {"),
        "review_prompt を入力欄へ流していない"
    );
    assert!(
        src.contains("self.review.ui(ui, &theme, &mut git_actions);"),
        "ReviewPanel を描いていない (描かないと画面に出ない)"
    );
    assert!(
        src.contains("self.review.set_workspace("),
        "ワークスペース切替に追随していない"
    );
    // 比較ベースの切替がコマンドから届く
    for k in [
        "\"staged\" => git_panel::ReviewBase::Staged",
        "\"unstaged\" => git_panel::ReviewBase::Unstaged",
    ] {
        assert!(src.contains(k), "{k} が無い");
    }
}

/// 新しいコマンドがすべてパレットに並び、ディスパッチ先がある。
#[test]
fn 新しいコマンドがパレットから届く() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let list = src
        .split("fn palette_builtin_cmds(&self) -> Vec<(String, String, String, Cmd)> {")
        .nth(1)
        .expect("パレットの一覧がある");
    let router = src
        .split("fn apply_cmd(&mut self, cmd: Cmd, ctx: &egui::Context) {")
        .nth(1)
        .expect("ディスパッチがある");
    for c in [
        "Cmd::OpenReview",
        "Cmd::SetReviewBase",
        // ── 差分ビュー (並列 / 変更ジャンプ) ──
        "Cmd::ToggleDiffView",
        "Cmd::DiffNextChange",
        "Cmd::DiffPrevChange",
        "Cmd::ToggleFold",
        "Cmd::FoldAll",
        "Cmd::UnfoldAll",
        "Cmd::FoldLevel",
        "Cmd::ToggleBookmark",
        "Cmd::NextBookmark",
        "Cmd::PrevBookmark",
        "Cmd::ClearBookmarks",
        "Cmd::ReopenClosedTab",
        "Cmd::ToggleTableView",
        "Cmd::LspCompletion",
        "Cmd::LspReferences",
        "Cmd::LspSymbols",
        "Cmd::LspRename",
        "Cmd::LspFormat",
        "Cmd::LspCodeAction",
        "Cmd::LspSignatureHelp",
        "Cmd::ToggleLspHighlight",
        "Cmd::ToggleFormatOnSave",
        // ── 第 3 次配線 ──
        "Cmd::RestartTutorial",
        "Cmd::OpenApprovals",
        "Cmd::OpenApprovalAudit",
        // ── MCP サーバ管理 ──
        "Cmd::OpenMcp",
        // ── Skills / slash command 管理 ──
        "Cmd::OpenSkills",
        "Cmd::AddCursorAbove",
        "Cmd::AddCursorBelow",
        "Cmd::SelectAllOccurrences",
        "Cmd::SelectNextOccurrence",
        "Cmd::ColumnSelectStart",
        "Cmd::ColumnSelectFinish",
        "Cmd::ClearMultiCursor",
        "Cmd::MultiPaste",
        "Cmd::ReopenWithEncoding",
        "Cmd::SaveWithEncoding",
        // ── エージェントデッキ ──
        "Cmd::ToggleDeck",
        // ── エディタの分割 ──
        "Cmd::SplitEditorRight",
        "Cmd::SplitEditorDown",
        "Cmd::UnsplitEditor",
        "Cmd::FocusNextPane",
        "Cmd::MoveTabToNextPane",
        // ── ミニマップ / ブレッドクラム ──
        "Cmd::ToggleMinimap",
        "Cmd::ToggleBreadcrumbs",
        // ── レート制限の自動フェイルオーバー ──
        "Cmd::ToggleFailover",
        // ── Git blame ──
        "Cmd::ToggleGitBlame",
        // ── 保存時のクリーンアップ / 選択範囲への編集コマンド ──
        "Cmd::ToggleTrimFinalNewlinesOnSave",
        "Cmd::TransformCase",
        "Cmd::SortLines",
        "Cmd::DedupeLines",
        "Cmd::FormatJsonSelection",
    ] {
        assert!(list.contains(c), "{c} がコマンドパレットに無い");
        assert!(router.contains(c), "{c} のディスパッチが無い");
    }
}

/// 第 4 次配線: `lsp.rs` に実装済みだった機能が UI から実際に呼ばれている。
///
/// `cargo check` の `never used` は「作ったのに繋いでいない」を一度は
/// 捕まえるが、あとから配線だけ外れても警告は出ない (別の場所から
/// 呼ばれ続けるため)。呼び出し側・到達経路・描画のどれが欠けても
/// ここで落ちるようにして、未配線への逆戻りを止める。
#[test]
fn lspのコードアクションと引数ヒントとハイライトが配線されている() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    // ① クライアント API を実際に叩いている
    for call in [
        "c.request_code_actions(&path, &range, &picked)",
        "c.poll_code_actions()",
        "c.execute_command(cmd)",
        "c.request_signature_help(&path, pos)",
        "c.poll_signature_help()",
        "c.request_document_highlight(&path, pos)",
        "c.poll_document_highlight()",
        "c.request_range_formatting(&path, &range, &opts)",
    ] {
        assert!(src.contains(call), "{call} を呼んでいる場所が無い");
    }
    // ② 到達経路 (キーバインド / パレット / 問題パネルの💡ボタン)
    for route in [
        "BindAction::LspCodeAction",
        "BindAction::LspSignatureHelp",
        "Cmd::LspCodeAction => self.lsp_code_actions()",
        "Cmd::LspSignatureHelp => self.lsp_signature_help()",
        "fix = Some((path.clone(), d.line, d.col));",
    ] {
        assert!(src.contains(route), "{route} の到達経路が無い");
    }
    // ③ 描いていなければ画面には出ない
    for draw in [
        "self.lsp_actions_tick(ctx);",
        "self.lsp_code_actions_popup(ctx);",
        "self.lsp_signature_popup(ctx);",
        "ui.painter().rect_filled(rect, 2.0, occ_color);",
    ] {
        assert!(src.contains(draw), "{draw} が呼ばれていない (画面に出ない)");
    }
    // ④ 整形は「選択があれば範囲整形」へ分岐している (経路は増やさない)
    assert!(
        src.contains("Some(range) if caps.range_formatting =>"),
        "選択時に rangeFormatting へ振り分けていない"
    );
}

/// ミニマップとブレッドクラムが「画面から到達できる」ことと、
/// **アイドル時に再構築も再描画要求もしない**ことをソースで固定する。
///
/// ソースを読むテストなので改行は正規化する (Windows は CRLF)。
/// **アイドル時に誰が描かせているかを、必ず記録できる形に保つ。**
///
/// 素の `ctx.request_repaint()` を直に呼ぶと、アイドルで CPU が回って
/// いても出所が分からず、仮説から入って外し続けることになる
/// (実際に 3 回外した記録がある)。app.rs の要求は必ず
/// `perf::repaint` / `perf::repaint_after` を通す。
///
/// 改行は正規化する (Windows のチェックアウトは CRLF)。
#[test]
fn 再描画の要求は必ず出所つきで行う() {
    let src = crate::app::SRC.replace("\r\n", "\n");
    // コメント行と、この検査自身は除いて数える。
    let bad: Vec<&str> = src
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with("/*") && !t.contains("crate::perf::repaint")
        })
        .filter(|l| l.contains(".request_repaint()") || l.contains(".request_repaint_after("))
        .filter(|l| !l.contains("contains(") && !l.contains("assert"))
        .collect();
    assert!(
        bad.is_empty(),
        "出所を記録しない再描画要求がある (perf::repaint / repaint_after を通すこと):\n{}",
        bad.join("\n")
    );
}

#[test]
fn ミニマップとブレッドクラムがアイドルを増やさない配線になっている() {
    let src = crate::app::SRC.replace("\r\n", "\n");
    let menu = include_str!("../menu_bar.rs").replace("\r\n", "\n");

    // 到達経路: 表示メニュー
    for c in ["Cmd::ToggleMinimap", "Cmd::ToggleBreadcrumbs"] {
        assert!(menu.contains(c), "{c} が表示メニューから押せない");
    }
    // 到達経路: エディタ本体の描画
    assert!(
        src.contains("self.breadcrumb_bar(ui);"),
        "ブレッドクラムを描いていない (描かないと画面に出ない)"
    );
    assert!(
        src.contains("crate::minimap::paint("),
        "ミニマップを描いていない (描かないと画面に出ない)"
    );
    assert!(
        src.contains("crate::minimap::minimap_visible("),
        "幅による自動非表示の純関数を通していない"
    );

    // 再構築はキャッシュキーが変わったときだけ (毎フレームではない)
    assert!(
        src.contains("if mm_on && minimap.as_ref().map(|(k, _)| *k) != Some(body_key)"),
        "ミニマップの再構築がキー比較で守られていない (毎フレーム再生成になる)"
    );
    // ミニマップ/ブレッドクラムのために再描画を要求していない
    let (_, editor_src) = src
        .split_once("fn code_editor_ui(&mut self, ui: &mut egui::Ui) {")
        .expect("code_editor_ui がある");
    let (editor_src, _) = editor_src
        .split_once("\n    // ─── UI: palette ")
        .expect("code_editor_ui の終わり");
    assert!(
        !editor_src.contains("request_repaint"),
        "エディタ描画が再描画を要求している (アイドルで CPU を焼く)"
    );
    for m in [
        include_str!("../minimap.rs").replace("\r\n", "\n"),
        include_str!("../breadcrumb.rs").replace("\r\n", "\n"),
    ] {
        assert!(
            !m.contains("request_repaint"),
            "ミニマップ / ブレッドクラムが再描画を要求している"
        );
    }
}

/// テストで使う実物の巨大ソース = このリポジトリの `src/app` モジュール一式。
/// **これがユーザーの報告した現物** (2MB / 43,000 行超)。
/// 分割前は 1 枚の `src/app.rs` だったので、同じ中身を [`crate::app::SRC`] で見る。
fn 実物の巨大ソース() -> String {
    crate::app::SRC.replace("\r\n", "\n")
}

/// 実物の `src/app.rs` を開いて **20,000 行目付近に色が付く**ことを見る。
///
/// 素の `Highlighter::layout_job` は 400KB を超えると色を丸ごと捨てて
/// 白一色を返すので、`layout_job_visible` へ繋ぎ替えていないとここが
/// 1 色になって落ちる (= ユーザーが報告したバグそのもの)。
#[test]
fn 巨大ファイルの二万行目に色が付く() {
    let src = 実物の巨大ソース();
    let lines: Vec<&str> = src.split_inclusive('\n').collect();
    let target = 20_000usize;
    let rows = 40usize;
    assert!(
        lines.len() > target + rows,
        "src/app.rs が 20,000 行より短い ({} 行)",
        lines.len()
    );
    let start: usize = lines[..target].iter().map(|l| l.len()).sum();
    let end: usize = start
        + lines[target..target + rows]
            .iter()
            .map(|l| l.len())
            .sum::<usize>();
    let win = crate::highlight::snap_window(target, rows);
    assert!(
        win.start <= target && win.end >= target + rows,
        "可視域が対象行を含んでいない: {win:?}"
    );
    let hl = crate::highlight::shared();
    let v = hl.layout_job_visible(
        &src,
        "Rust",
        "base16-ocean.dark",
        egui::FontId::monospace(12.0),
        egui::Color32::WHITE,
        win,
    );
    let mut colors = std::collections::HashSet::new();
    for sec in &v.job.sections {
        if sec.byte_range.start < end && sec.byte_range.end > start {
            colors.insert(sec.format.color.to_array());
        }
    }
    assert!(
        colors.len() >= 3,
        "20,000 行目付近が {} 色しかない (色を捨てている)",
        colors.len()
    );
    // 可視域の**外**はまとめて 1 セクションで足りる (画面に出ないので
    // 色が要らない。セクションを作る費用だけが乗る)。
    let win_start_byte: usize = lines[..win.start].iter().map(|l| l.len()).sum();
    let outside = v
        .job
        .sections
        .iter()
        .filter(|s| s.byte_range.end <= win_start_byte)
        .count();
    assert!(
        outside <= 1,
        "可視域の手前を {outside} セクションに割っている (1 つで足りる)"
    );
}

/// 追い付き (`advance_to_visible`) は**追い付いたら止まる**。
/// 止まらなければ `code_editor_ui` が毎フレーム再描画を要求し続け、
/// アイドルの CPU がゼロにならない (設計原則 3)。
#[test]
fn 可視域の追い付きは終わったら本文に触れない() {
    let src = 実物の巨大ソース();
    let win = crate::highlight::snap_window(20_000, 40);
    let hl = crate::highlight::shared();
    let mut pumps = 0usize;
    loop {
        let a = hl.advance_to_visible(&src, "Rust", "base16-ocean.dark", egui::Color32::WHITE, win);
        if a.ready {
            break;
        }
        pumps += 1;
        assert!(pumps < 1_000_000, "追い付かない");
    }
    // 追い付いた後は本文を 1 行も舐めない = 再描画を要求しない条件。
    for _ in 0..8 {
        let a = hl.advance_to_visible(&src, "Rust", "base16-ocean.dark", egui::Color32::WHITE, win);
        assert!(a.ready, "一度追い付いたのに戻った");
        assert_eq!(
            a.scanned_lines, 0,
            "追い付いた後も本文を舐めている (アイドルで CPU を焼く)"
        );
    }
}

/// 可視域ハイライトが `code_editor_ui` に**繋がっている**ことと、
/// 再描画の要求が「追い付くまで」に限られていることを構造で固定する。
#[test]
fn 可視域ハイライトがエディタ描画へ繋がっている() {
    let src = crate::app::SRC.replace("\r\n", "\n");
    let (_, editor_src) = src
        .split_once("fn code_editor_ui(&mut self, ui: &mut egui::Ui) {")
        .expect("code_editor_ui がある");
    let (editor_src, _) = editor_src
        .split_once("\n    // ─── UI: palette ")
        .expect("code_editor_ui の終わり");
    assert!(
        editor_src.contains("hl.layout_job_visible("),
        "古い layout_job のままで、巨大ファイルの色が捨てられる"
    );
    assert!(
        !editor_src.contains("hl.layout_job("),
        "素の layout_job が残っている (可視域を渡さない経路)"
    );
    assert!(
        editor_src.contains("self.highlighter.advance_to_visible("),
        "毎フレームの追い付きを回していない"
    );
    assert!(
        editor_src.contains("crate::highlight::snap_window(top_disp_line, hl_rows)"),
        "可視域を snap_window で丸めていない (生の行番号は 495ms/フレームになる)"
    );
    assert!(
        editor_src.contains("galley_window_key(hl_windowed_prev, hl_win)"),
        "可視域を galley キーへ混ぜていない (スクロールしても色が追わない)"
    );
    // 分割中は 1 フレームでペインの数だけ走る。状態を 1 枠で持つと
    // 2 ペインが毎フレーム上書きし合い、どちらも galley を組み直し続ける。
    assert!(
        editor_src.contains("self.hl_windowed.get(&ed_id_early)")
            && editor_src.contains("self.hl_ready.get(&ed_id_early)"),
        "追い付き状態が (ペイン, バッファ) ごとになっていない"
    );
    // 再描画の要求は「まだ追い付いていない」枝の中だけ。
    let (_, after) = editor_src
        .split_once("if !adv.ready {")
        .expect("追い付き待ちの分岐がある");
    let (guarded, _) = after.split_once("\n            }").expect("分岐の終わり");
    assert!(
        guarded.contains("crate::perf::repaint(ui.ctx(), \"highlight-window\")"),
        "追い付き待ちで再描画を要求していない (色が出るまで固まる)"
    );
    assert_eq!(
        editor_src.matches("perf::repaint(").count(),
        1,
        "エディタ描画からの再描画要求は追い付き待ちの 1 か所だけにする"
    );
}

/// `current` が取りに行く行域は**カーソルの帯だけ**。
///
/// 全行ぶん (可視域 = 数百行) を取ってから 1 行だけ描くのでは、重さが
/// `all` と変わらず 3 段にした意味が無い。一方で 1 行きっかりにすると
/// カーソルを 1 行動かすたびに git が起きるので、帯へ丸める。
#[test]
fn current_の_blame_は帯ぶんしか取りに行かない() {
    // 帯の中ではキーが動かない = git が起きない
    let a = super::blame_current_range(0, 10_000);
    for l in 0..super::BLAME_CURRENT_BAND {
        assert_eq!(
            super::blame_current_range(l, 10_000),
            a,
            "{l} 行目で帯が動いた"
        );
    }
    // 帯をまたぐと動く
    assert_ne!(
        super::blame_current_range(super::BLAME_CURRENT_BAND, 10_000),
        a
    );
    // 取りに行く量は帯ぶんだけ (git::BLAME_BLOCK = 200 行より必ず小さい)
    for caret in [0usize, 1, 15, 16, 999, 9_999] {
        let (bs, be) = super::blame_current_range(caret, 10_000);
        assert!(bs >= 1 && be >= bs && be <= 10_000, "{caret}: {bs}..{be}");
        assert!(
            be - bs + 1 <= super::BLAME_CURRENT_BAND,
            "{caret}: {} 行も取りに行っている",
            be - bs + 1
        );
        assert!(
            bs <= caret + 1 && caret + 1 <= be,
            "{caret}: カーソル行 {} が範囲 {bs}..{be} の外",
            caret + 1
        );
        assert!(
            be - bs + 1 < crate::git::BLAME_BLOCK,
            "全行モードと同じ量を取っている"
        );
    }
    // 端: 空ファイル / 末尾を越えたカーソル / 1 行だけ
    assert_eq!(super::blame_current_range(0, 0), (1, 1));
    assert_eq!(super::blame_current_range(0, 1), (1, 1));
    assert_eq!(super::blame_current_range(99_999, 5), (1, 5));
}

/// 3 段の blame が**画面の配線まで**届いていること。
#[test]
fn blame_の三段が描画とコマンドへ繋がっている() {
    let src = crate::app::SRC.replace("\r\n", "\n");
    // 状態を書き換える入口は 1 つだけ (3 経路がここへ集まる)
    // needle は実行時に組み立てる (この行そのものが自分に一致しないため)
    let needle = format!(
        "pub(crate) fn set_blame_mode(&mut self, mode: {}::BlameMode)",
        "config"
    );
    assert_eq!(
        src.matches(&needle).count(),
        1,
        "段を決める入口が 1 つでない"
    );
    // 表示メニューの 1 項目は次の段へ回す
    assert!(
        src.contains("self.set_blame_mode(self.cfg.git_blame.next());"),
        "表示メニューから段を回せない"
    );
    // 取得と描画の両方がモードを見ている
    assert!(
        src.contains(
            "config::BlameMode::Current => blame_current_range(caret_src_line, line_count)"
        ),
        "current でも全行ぶん取りに行っている (重さが変わらない)"
    );
    assert!(
        src.contains("if blame_only_line.is_some_and(|l| l != *src)"),
        "current でも全行描いている"
    );
    // git を UI スレッドで待っていない (既存の非同期経路をそのまま使う)
    assert!(
        src.contains("self.blame.request(&top, &rel, key.clone())"),
        "blame の非同期経路を通っていない"
    );
}

/// スクロールバー帯の装飾が **egui 本来のドラッグを奪っていない**こと。
///
/// `ScrollArea` は `outer_scroll_bar_rect` へ `Sense::click_and_drag()` を
/// 先に置いている。後から `click_and_drag` を重ねると egui の hit_test が
/// こちらを選び、つまみのドラッグが「クリック位置へ飛ぶ」に変わる。
#[test]
fn スクロールバーの装飾がドラッグを奪っていない() {
    let src = crate::app::SRC.replace("\r\n", "\n");
    let (_, band) = src
        .split_once("ed_id.with(\"scrollbar_marks\"),")
        .expect("スクロールバー帯の当たり判定がある");
    let sense = band.lines().take(3).collect::<String>();
    assert!(
        sense.contains("egui::Sense::click()"),
        "帯が click 以外を掴んでいる (つまみのドラッグを奪う): {sense}"
    );
    // つまみの上のクリックは egui へ譲る
    assert!(
        src.contains(".filter(|pos| !deco.viewport.contains(*pos))"),
        "つまみの上のクリックでも飛ばしている"
    );
    // 二重描画の防止: ミニマップ表示中は帯を出さない
    assert!(
        src.contains("if !mm_on && sb_visible {"),
        "ミニマップ表示中に帯を二重に描いている / バーが無くても描いている"
    );
    // 印が 0 件なら 1 ピクセルも触らない (空白は作らない)
    assert!(
        src.contains("if !deco.marks.is_empty() {"),
        "印が 0 件でも帯を描いている"
    );
    // egui のつまみと二重になるビューポート枠は描かない
    let viewport_fill = format!("p.rect_filled(deco.{},", "viewport");
    assert!(
        !src.contains(&viewport_fill),
        "egui のつまみの上へビューポート枠を重ねている"
    );
}

/// 可視域を galley キーへ混ぜるのは「塗り分けが可視域に依存するとき」だけ。
/// 小さい文書まで混ぜると、512 行スクロールするたびに galley を丸ごと
/// 組み直す (実測 495ms/回)。
#[test]
fn galley_キーの可視域は窓が効くときだけ混ざる() {
    use crate::highlight::Window;
    let cases = [
        Window { start: 0, end: 512 },
        Window {
            start: 19_968,
            end: 20_992,
        },
        Window {
            start: usize::MAX - 1,
            end: usize::MAX,
        },
    ];
    for w in cases {
        assert_eq!(
            super::galley_window_key(false, w),
            (0, 0),
            "窓が効かない文書で可視域を混ぜている: {w:?}"
        );
        assert_eq!(
            super::galley_window_key(true, w),
            (w.start, w.end),
            "窓が効く文書で可視域を混ぜていない: {w:?}"
        );
    }
    // 同じ画面でも行が 1 つ動いただけでは鍵が動かない (512 行単位へ丸める)
    let a = crate::highlight::snap_window(20_000, 40);
    let b = crate::highlight::snap_window(20_001, 40);
    assert_eq!(
        super::galley_window_key(true, a),
        super::galley_window_key(true, b),
        "1 行スクロールで galley を組み直す鍵になっている"
    );
}

/// フェイルオーバーが「作ったのに繋いでいない」状態で終わらないことを、
/// ソースの構造で固定する。UI から到達できない実装は未完成なので、
/// 到達経路 (パレット / 設定トグル / 状態表示) と駆動点を全部見る。
#[test]
fn 自動フェイルオーバーが画面と駆動点に繋がっている() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    // 毎フレームの駆動 (段を進める) と、レート制限イベントからの起動。
    assert!(
        src.contains("self.failover_tick();"),
        "毎フレームの駆動が無い"
    );
    assert!(
        src.contains("self.failover_on_rate_limit(&title, &line, ctx);"),
        "レート制限イベントから呼ばれていない"
    );
    // 設定トグル (📊 プラン使用量ウィンドウ) と、そこからの保存。
    let win = src
        .split("fn quota_window_ui(&mut self, ctx: &egui::Context) {")
        .nth(1)
        .expect("使用量ウィンドウがある");
    assert!(
        win.contains("🔁 レート制限で自動的に切り替える"),
        "設定トグルがウィンドウに無い"
    );
    assert!(win.contains("fo_stage"), "今どの段にいるかを出していない");
    assert!(
        src.contains("fn set_failover_enabled(&mut self, on: bool)")
            && src.contains("config::save_state(&self.cfg);"),
        "切替結果が永続化されていない"
    );
    // セッションを閉じたら段を忘れる (ID 再利用の巻き添え防止)。
    let close = src
        .split("fn close_agent(&mut self, i: usize) {")
        .nth(1)
        .expect("close_agent がある");
    assert!(
        close.contains("self.failover.forget_session(id);"),
        "閉じたセッションの段を忘れていない"
    );
}

/// 切替は**現行セッションを殺さない**。フェイルオーバー経路に kill / remove /
/// restart が紛れ込んでいないことをソースで固定する
/// (終了済みセッションへ kill を撃たない既存ガードの regress 防止も兼ねる)。
#[test]
fn フェイルオーバーは現行セッションを殺さない() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn failover_on_rate_limit(")
        .nth(1)
        .and_then(|s| {
            s.split("\n    /// 切替先プリセットでセッションを起動し")
                .next()
        })
        .expect("failover_on_rate_limit の本体がある");
    for forbidden in ["kill", "close_agent", "agents.remove", "agents.restart"] {
        assert!(
            !body.contains(forbidden),
            "フェイルオーバーが {forbidden} を呼んでいる (現行セッションは残すこと)"
        );
    }
}

/// Git blame は「パレット」「表示メニュー」「config」の 3 経路から届き、
/// **既定は OFF**。どれかが切れたら気付けるようにする。
#[test]
fn git_blameは既定offで3経路から届く() {
    use crate::config::BlameMode;
    assert_eq!(
        crate::config::Config::default().git_blame,
        BlameMode::Off,
        "既定は OFF (勝手に git が走らない)"
    );
    let menu = include_str!("../menu_bar.rs").replace("\r\n", "\n");
    assert!(
        menu.contains("Cmd::ToggleGitBlame"),
        "表示メニューから届いていない"
    );
    // 3 段すべてが**パレットから 1 手で**選べる (循環しか無いと選べない)。
    let ids: Vec<&str> = crate::feature::palette_entries(&crate::keybinds::FeatureBinds::default())
        .iter()
        .filter_map(|(_, _, _, c)| match c {
            crate::palette::Cmd::Feature(id) => Some(*id),
            _ => None,
        })
        .collect();
    for id in ["blame.off", "blame.current", "blame.all"] {
        assert!(ids.contains(&id), "{id} がパレットに出ていない");
    }
    // config (設定画面) 側は 3 択で出る。
    let def = crate::config::setting_defs()
        .iter()
        .find(|d| d.key == "git_blame")
        .expect("設定一覧に git_blame がある");
    match def.kind {
        crate::config::SettingKind::Choice(opts) => assert_eq!(
            opts,
            ["off", "current", "all"],
            "設定画面の選択肢が 3 段になっていない"
        ),
        _ => panic!("git_blame が Choice ではない (トグルのままでは選べない)"),
    }
    // 表示メニューの 1 項目は 3 段を順に回す (どの段からも次へ行ける)。
    let mut m = BlameMode::Off;
    let mut seen: Vec<BlameMode> = Vec::new();
    for _ in 0..3 {
        assert!(!seen.contains(&m), "循環が 3 段に届いていない");
        seen.push(m);
        m = m.next();
    }
    assert_eq!(m, BlameMode::Off, "3 回で元へ戻らない");
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let update = src
        .split("fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {")
        .nth(1)
        .expect("update がある");
    assert!(
        update.contains("self.blame.poll();"),
        "blame の結果を update から取り込んでいない"
    );
    assert!(
        update.contains("self.blame.busy()"),
        "実行中だけ再描画を予約する仕組みが無い (アイドルで回り続ける)"
    );
    // ガターの描画とクリックの配線
    assert!(src.contains("git::blame_row_label"), "ガターへ描いていない");
    assert!(
        src.contains("Align2::RIGHT_TOP"),
        "blame ラベルを右寄せにしていない (短いほど行番号から離れる)"
    );
    assert!(
        src.contains("self.open_commit_diff(&sha)"),
        "クリックで差分を開いていない"
    );
}

/// タブのドラッグ並べ替えが**実際に配線されている**こと。
#[test]
fn タブは掴んで並べ替えられる() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    assert!(
        src.contains("egui::Sense::click_and_drag()"),
        "タブが click_and_drag になっていない"
    );
    assert!(
        src.contains("self.reorder_tab(from, to);"),
        "落とした結果が並べ替えへ繋がっていない"
    );
    assert!(
        src.contains("fn reorder_tab(&mut self, from: usize, to: usize)"),
        "並べ替えの実体が無い"
    );
}

/// **タブのピン留め / プレビュー / MRU 切替が実際に配線されている**こと。
///
/// 実装はしたが画面から届かない (= 未完成) を検出する番人。
/// 純粋なロジックは `editor_split::tests` が固定しているので、ここは
/// 「その判断が描画と入力に繋がっているか」だけを見る。
#[test]
fn タブのピン留めとプレビューは画面から届く() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    // 到達経路: 右クリックメニュー / パレット / ⌃Tab
    assert!(
        src.contains("pin_req = Some(i);"),
        "タブの右クリックからピン留めできない"
    );
    assert!(
        src.contains("Cmd::TogglePinTab =>"),
        "パレットのピン留めが実行されない"
    );
    assert!(
        src.contains("self.tab_switcher_tick(ctx);") && src.contains("self.tab_switcher_ui(ctx);"),
        "MRU タブ切替の確定と候補一覧が update から呼ばれていない"
    );
    // ツリー / パレットの 1 回クリックはプレビューで開く
    assert!(
        src.contains("self.open_path_preview(&p);"),
        "ツリーの 1 回クリックがプレビューに繋がっていない"
    );
    // ピン留めタブには「×」を出さない (誤って閉じないための機能なので)
    let body = src
        .split("fn editor_tab_strip(")
        .nth(1)
        .expect("タブ列の描画がある");
    let guard = body.find("if is_pinned {").expect("ピン留めの分岐がある");
    let cross = body
        .find("RichText::new(\"×\")")
        .expect("閉じるボタンがある");
    assert!(guard < cross, "ピン留めタブでも閉じるボタンを描いてしまう");
    // 落とし先はピン境界でクランプする
    assert!(
        body.contains("editor_split::clamp_reorder("),
        "ドラッグの落とし先がピン境界でクランプされていない"
    );
    // アクティブタブへの追従は純関数が決めた矩形から
    assert!(
        body.contains("editor_split::tab_rects(") && body.contains("ui.scroll_to_rect("),
        "アクティブタブへ自動スクロールしていない"
    );
}

// ─────────────────────────────────────────────────────────────────
// Hot Exit の差分表示 / 設定画面への到達経路
// ─────────────────────────────────────────────────────────────────

#[test]
fn 復元とディスクの差分は追加と削除を並べる() {
    let disk = "one\ntwo\nthree\n";
    let saved = "one\nTWO\nthree\nfour\n";
    let d = unified_lines(disk, saved, HOTEXIT_DIFF_MAX_CELLS);
    assert!(d.contains("-two\n"), "{d}");
    assert!(d.contains("+TWO\n"), "{d}");
    assert!(d.contains(" one\n") && d.contains(" three\n"), "{d}");
    assert!(d.contains("+four\n"), "{d}");
    // 片側が空でも壊れない
    assert!(unified_lines("", "a\n", HOTEXIT_DIFF_MAX_CELLS).contains("+a\n"));
    assert!(unified_lines("a\n", "", HOTEXIT_DIFF_MAX_CELLS).contains("-a\n"));
    assert!(unified_lines("", "", HOTEXIT_DIFF_MAX_CELLS).contains("違いはありません"));
    // CJK と絵文字を含む行も落ちない
    let d = unified_lines("日本語\n", "日本語👨‍👩‍👧‍👦\n", HOTEXIT_DIFF_MAX_CELLS);
    assert!(d.contains("-日本語\n") && d.contains("+日本語👨‍👩‍👧‍👦\n"), "{d}");
}

#[test]
fn 巨大な差分は計算せず行数だけ出す() {
    // 上限を超えたら LCS を諦める (フレームを落とさない)
    let a: String = (0..200).map(|i| format!("a{i}\n")).collect();
    let b: String = (0..200).map(|i| format!("b{i}\n")).collect();
    let d = unified_lines(&a, &b, 100);
    assert!(d.contains("大きすぎるため"), "{d}");
    assert!(d.contains("200"), "{d}");
    // 上限内なら普通に差分が出る
    assert!(!unified_lines(&a, &b, 1_000_000).contains("大きすぎるため"));
}

/// 設定画面には**必ず**到達経路がある (パレット + メニュー)。
/// 実装したのに繋いでいない、を構造で検出する。
#[test]
fn 設定画面はパレットとメニューから開ける() {
    let app_src = crate::app::SRC.replace("\r\n", "\n");
    let menu_src = include_str!("../menu_bar.rs").replace("\r\n", "\n");
    let head = app_src
        .split("\n#[cfg(test)]\nmod tests {")
        .next()
        .unwrap_or("");
    assert!(
        head.contains("Cmd::OpenSettings => self.settings_open = true"),
        "設定画面を開くコマンドの処理が無い"
    );
    assert!(
        palette_body(&app_src).contains("Cmd::OpenSettings"),
        "パレットに設定画面が無い"
    );
    assert!(
        menu_src.contains("Cmd::OpenSettings"),
        "メニューに設定画面が無い"
    );
    // config.toml を直接編集する口も必ず残す (GUI で表現しきれない設定用)
    assert!(
        palette_body(&app_src).contains("Cmd::OpenConfig"),
        "config.toml を開く口が消えている"
    );
    assert!(
        head.contains("self.settings_window(ctx);"),
        "設定画面が描画されていない"
    );
    assert!(
        head.contains("self.hotexit_conflict_window(ctx);"),
        "Hot Exit の競合ダイアログが描画されていない"
    );
}

/// Hot Exit が「編集したときだけ」働くことをソースで固定する。
/// 常時 I/O / 常時再描画は設計原則 3 (アイドル時のコストはゼロ) 違反。
#[test]
fn hot_exitは編集があったときだけ退避する() {
    let src = crate::app::SRC.replace("\r\n", "\n");
    let head = src
        .split("\n#[cfg(test)]\nmod tests {")
        .next()
        .unwrap_or("");
    assert!(
        head.contains("self.hotexit_tick(ctx);"),
        "退避の刻みが update から呼ばれていない"
    );
    // 指紋は本文をハッシュしない (巨大ファイルで毎フレーム走らせない)
    let at = head
        .find("fn hotexit_fingerprint(&self)")
        .expect("hotexit_fingerprint が無い");
    let mut end = (at + 900).min(head.len());
    while end < head.len() && !head.is_char_boundary(end) {
        end += 1;
    }
    let body = &head[at..end];
    assert!(
        !body.contains("hash_str(") && !body.contains(".dirty()"),
        "指紋が本文をハッシュしている (毎フレーム全文を舐めることになる)"
    );
    assert!(
        body.contains("history.revision()"),
        "履歴の安い版数を見ていない"
    );
    // 保存したら即座に退避を掃除する (次の間隔まで残さない)
    assert!(
            head.contains("// 保存した本文の退避はもう要らない (ゴミを残さない)\n                self.hotexit_flush();")
                || head.contains("// 保存した本文の退避はもう要らない (ゴミを残さない)\n            self.hotexit_flush();"),
            "保存後に退避を掃除していない"
        );
}

/// `palette_builtin_cmds` の本体だけを切り出す。
fn palette_body(src: &str) -> &str {
    let body = src
        .split("fn palette_builtin_cmds(&self) -> Vec<(String, String, String, Cmd)> {")
        .nth(1)
        .expect("パレットの一覧がある");
    let end = body.find("\n    }\n").expect("一覧の終わり");
    &body[..end]
}

/// `at` 以降にある最初の `Cmd::識別子` を返す。
fn next_cmd_ident(src: &str, at: usize) -> Option<(usize, String)> {
    let rel = src[at..].find("Cmd::")?;
    let start = at + rel + "Cmd::".len();
    let ident: String = src[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!ident.is_empty()).then_some((start, ident))
}

/// **パレットに並ぶ全コマンドが、実際に何かを実行する腕へ届く。**
///
/// 「パレットには出るが押しても無反応」を潰すための番人。
/// ディスパッチが無い / 空の腕になっているものは 1 つでも落とす。
#[test]
fn パレットの全コマンドが実行される腕へ届く() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let list = palette_body(src);
    // 上位ディスパッチ (`apply_cmd` の本体) だけを見る。ここから先を全部
    // 対象にすると、パレットの一覧自身に当たって素通りしてしまう。
    let router_all = src
        .split("fn apply_cmd(&mut self, cmd: Cmd, ctx: &egui::Context) {")
        .nth(1)
        .expect("ディスパッチがある");
    let router = &router_all[..crate::app::method_end(router_all)];

    let mut seen: Vec<String> = Vec::new();
    let mut at = 0usize;
    while let Some((next, ident)) = next_cmd_ident(list, at) {
        at = next + ident.len();
        if !seen.contains(&ident) {
            seen.push(ident);
        }
    }
    assert!(
        seen.len() > 80,
        "パレットの走査が壊れている ({} 件)",
        seen.len()
    );

    let mut broken: Vec<String> = Vec::new();
    for name in &seen {
        if !router.contains(&format!("Cmd::{name}")) {
            broken.push(format!("{name}: ディスパッチが無い"));
        }
        // 何もしない腕 (書いたつもりで繋いでいない) も落とす
        for noop in [
            format!("Cmd::{name} => {{}}"),
            format!("Cmd::{name} => (),"),
            format!("Cmd::{name}(..) => {{}}"),
            format!("Cmd::{name}(_) => {{}}"),
        ] {
            if src.contains(&noop) {
                broken.push(format!("{name}: 何もしない腕になっている"));
            }
        }
    }
    assert!(
        broken.is_empty(),
        "パレットに出るのに効かないコマンドがある: {broken:?}"
    );
}

/// **インレイヒントが画面に繋がっていて、到達経路がある。**
///
/// 「LSP からヒントは届いているのに画面に出ない」「実装したがどこからも
/// 切り替えられない」を潰すための番人。描画・到達経路・本文の不可侵を見る。
/// ソースを読むテストなので改行は正規化する (Windows は CRLF)。
#[test]
fn インレイヒントが画面に繋がっている() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let (_, body) = src
        .split_once("fn code_editor_ui(&mut self, ui: &mut egui::Ui) {")
        .expect("code_editor_ui がある");
    let (body, _) = body
        .split_once("\n    // ─── UI: palette ")
        .expect("code_editor_ui の終わり");

    // ① 本文描画の中で、純関数の結果を実際に塗っている
    for draw in [
        "diagview::inlay_line_text(",
        "diagview::inlay_marks(",
        "inlay_colors[(v.kind == lsp::INLAY_KIND_PARAMETER) as usize]",
    ] {
        assert!(
            body.contains(draw),
            "{draw} が本文描画に無い (画面に出ない)"
        );
    }
    // ② 色はテーマ経由 (ベタ書き禁止)
    assert!(
        body.contains("diagview::inlay_color(&self.theme,"),
        "インレイヒントの色をテーマから取っていない"
    );
    // ③ **本文 (galley) にヒントを混ぜていない。** 混ぜるとキャレット・選択・
    //    クリック位置が壊れる。差し込む形の操作が本文描画に無いことを見る。
    //    禁止パターンは分割して組み立てる (このテスト自身の文字列に当たらない)。
    for forbidden in [
        format!("text.{}(v.at", "insert_str"),
        format!("target.set(with_{}", "inlay"),
        format!("{}_apply_to_text(", "inlay"),
    ] {
        assert!(
            !src.contains(&forbidden),
            "{forbidden}: 本文へヒントを混ぜている (galley の char 添字が壊れる)"
        );
    }
    // ④ 行末の診断と重ならないよう押し出している
    assert!(
        body.contains("inlay_row_end"),
        "行末の診断メッセージと重なりを避けていない"
    );
    // ⑤ 到達経路: パレット / 設定 / 要求
    for route in [
        "Cmd::ToggleInlayHints",
        "self.cfg.inlay_hints",
        "self.refresh_inlay_hints(text_hash);",
        "client.request_inlay_hints(",
    ] {
        assert!(src.contains(route), "{route} の到達経路が無い");
    }
    // ⑥ 要求は「飛行中でないとき」だけ (毎フレーム撃たない = 設計原則 3)
    let refresh = src
        .split("fn refresh_inlay_hints(&mut self, text_hash: u64) {")
        .nth(1)
        .expect("refresh_inlay_hints がある");
    assert!(
        refresh.contains("if !client.inlay_in_flight(&path)"),
        "同じ版の要求を毎フレーム撃ってしまう"
    );
}

/// **診断のインライン表示と対応括弧の強調が画面に繋がっている。**
///
/// 「LSP から範囲は届いているのにガターの印しか出ない」「実装したが
/// どこからも押せない」を潰すための番人。描画・到達経路・排他を全部見る。
/// ソースを読むテストなので改行は正規化する (Windows は CRLF)。
#[test]
fn 診断のインライン表示と対応括弧が画面に繋がっている() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    // 本文描画の中だけを見る (テスト自身の文字列に当たらないようにする)
    let (_, body) = src
        .split_once("fn code_editor_ui(&mut self, ui: &mut egui::Ui) {")
        .expect("code_editor_ui がある");
    let (body, _) = body
        .split_once("\n    // ─── UI: palette ")
        .expect("code_editor_ui の終わり");

    // ① 範囲付きの診断を持ち、本文へ波線・括弧・行末メッセージを描いている
    for draw in [
        "diagview::squiggle_points(",
        "egui::Shape::line(pts,",
        "bracket_fill[k]",
        "diagview::inline_message(",
        "diag_colors[(sp.severity.clamp(1, 4) - 1) as usize]",
    ] {
        assert!(
            body.contains(draw),
            "{draw} が本文描画に無い (画面に出ない)"
        );
    }
    // ② 括弧の相手探しは既存のマッチャ 1 つだけ (app.rs に複製しない)
    assert!(
        body.contains("diagview::bracket_hl("),
        "対応括弧を求めていない"
    );
    assert!(
        !src.contains(&format!("fn {}", "matching_bracket")),
        "app.rs に括弧マッチャを複製している (editor_ops のものを使うこと)"
    );
    // ③ 色は必ずテーマ経由 (severity → 色のベタ書きを禁じる)
    assert!(
        body.contains("diagview::severity_color(&self.theme, 1)"),
        "診断の色をテーマから取っていない"
    );
    // ④ 到達経路: キーバインド / パレット / 設定
    for route in [
        "BindAction::NextProblem",
        "BindAction::PrevProblem",
        "Cmd::NextProblem => self.goto_diagnostic(ctx, true)",
        "Cmd::PrevProblem => self.goto_diagnostic(ctx, false)",
        "Cmd::ToggleInlineDiagnostics",
        "self.cfg.inline_diagnostics",
        "self.diag_hover_popup(ctx);",
    ] {
        assert!(src.contains(route), "{route} の到達経路が無い");
    }
    // ⑤ 診断ホバーと LSP ホバーは排他 (同じ場所へ 2 枚重ねない)
    let hover = body
        .split("match hover_hit {")
        .nth(1)
        .expect("ホバーの分岐がある");
    assert!(
        hover.contains("self.diag_hover = Some(") && hover.contains("self.lsp_hover.dismiss()"),
        "診断が当たったフレームで LSP ホバーを止めていない"
    );
}

/// **パレットが表示する打鍵は、その行が実行する `Cmd` の `BindAction` から作る。**
///
/// ベタ書きに戻すと再割り当てで嘘になり、別のアクションの打鍵を貼ると
/// 「書いてある通りに押しても違うことが起きる」。両方をここで止める。
#[test]
fn パレットの打鍵表示は実行するコマンドと一致する() {
    // (BindAction, その行が実行する Cmd)
    const PAIRS: &[(&str, &str)] = &[
        // ── 第 4 次で足したもの (分割 / 差分ジャンプ / LSP) ──
        // 名前が食い違う 1 件だけ注釈する: ⌘2 は「次のペインへ」を
        // 実行する (ペイン 2 を名指しで選ぶのではない)。
        ("SplitEditorRight", "SplitEditorRight"),
        ("SplitEditorDown", "SplitEditorDown"),
        ("FocusPane2", "FocusNextPane"),
        ("DiffNextChange", "DiffNextChange"),
        ("DiffPrevChange", "DiffPrevChange"),
        ("NextProblem", "NextProblem"),
        ("PrevProblem", "PrevProblem"),
        ("LspCodeAction", "LspCodeAction"),
        ("LspSignatureHelp", "LspSignatureHelp"),
        ("Save", "Save"),
        ("SaveAs", "SaveAs"),
        ("NewFile", "NewFile"),
        ("NewWindow", "NewWindow"),
        ("CloseTab", "CloseTab"),
        ("Find", "OpenFind"),
        ("ToggleTerminal", "ToggleTerminal"),
        ("ToggleCockpit", "ToggleCockpit"),
        ("ToggleKanban", "ToggleKanban"),
        ("ToggleDeck", "ToggleDeck"),
        ("ToggleMdPreview", "ToggleMdPreview"),
        ("ToggleSidebar", "ToggleSidebar"),
        ("ZoomIn", "ZoomIn"),
        ("ZoomOut", "ZoomOut"),
        ("ZoomReset", "ZoomReset"),
        ("FileZoomIn", "FileZoomIn"),
        ("FileZoomOut", "FileZoomOut"),
        ("FileZoomReset", "FileZoomReset"),
        ("OpenFile", "OpenFileDialog"),
        ("SaveAll", "SaveAll"),
        ("GlobalSearch", "GlobalSearch"),
        ("GlobalReplace", "GlobalReplace"),
        ("OpenReplace", "OpenReplace"),
        ("GoToLine", "GoToLine"),
        ("GoToDefinition", "GoToDefinition"),
        ("GoToBracket", "GoToBracket"),
        ("NavBack", "NavBack"),
        ("NavForward", "NavForward"),
        ("NextTab", "NextTab"),
        ("PrevTab", "PrevTab"),
        ("NewTerminal", "NewTerminal"),
        ("RunBuildTask", "RunBuildTask"),
        ("ToggleProblems", "ToggleProblems"),
        ("ToggleFullScreen", "ToggleFullScreen"),
        ("ToggleFold", "ToggleFold"),
        ("UnfoldAll", "UnfoldAll"),
        ("ToggleBookmark", "ToggleBookmark"),
        ("MarkToggleMnemonic", "MarkToggleMnemonic"),
        ("MarksPanel", "MarksPanel"),
        ("MarkJump", "MarkJump"),
        ("ReopenClosedTab", "ReopenClosedTab"),
        ("LspCompletion", "LspCompletion"),
        ("LspReferences", "LspReferences"),
        ("LspSymbols", "LspSymbols"),
        ("LspRename", "LspRename"),
        ("LspFormat", "LspFormat"),
        ("SelectNextOccurrence", "SelectNextOccurrence"),
        ("DiffNextFile", "DiffNextFile"),
        ("DiffPrevFile", "DiffPrevFile"),
        ("KeybindEditor", "ShowShortcuts"),
        ("FollowAgent", "ToggleFollowAgent"),
        ("FollowResume", "ResumeFollowAgent"),
        ("NextUnread", "NextUnreadAgent"),
        ("DeferUnread", "DeferUnreadAgent"),
        ("ToggleUnread", "ToggleUnreadAgent"),
    ];
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let list = palette_body(src);

    let mut checked = 0usize;
    let mut wrong: Vec<String> = Vec::new();
    let needle = "fmt_key(BindAction::";
    let mut at = 0usize;
    while let Some(rel) = list[at..].find(needle) {
        let start = at + rel + needle.len();
        let action: String = list[start..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        at = start + action.len();
        let Some((_, cmd)) = next_cmd_ident(list, at) else {
            wrong.push(format!("{action}: 同じ行に Cmd が無い"));
            continue;
        };
        checked += 1;
        match PAIRS.iter().find(|(a, _)| *a == action) {
            Some((_, want)) if *want == cmd => {}
            Some((_, want)) => wrong.push(format!(
                "BindAction::{action} の打鍵を Cmd::{cmd} の行に出している (正しくは Cmd::{want})"
            )),
            None => wrong.push(format!(
                "BindAction::{action} / Cmd::{cmd} の対応が表に無い (表を更新すること)"
            )),
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    assert_eq!(
        checked,
        PAIRS.len(),
        "打鍵を出しているパレット行の数が表と合わない \
             (ベタ書きへ戻したか、行を消した)"
    );
}
// ─── ズーム (画面全体 / ファイル単位) ────────────────────────────────

/// ズームの 6 コマンドすべてに、UI から届く経路が最低 1 本ある。
///
/// 「実装したがどこからも押せない」を防ぐための配線チェック
/// (パレット / メニュー / キーバインド / ディスパッチの 4 点)。
#[test]
fn ズームは画面全体とファイル単位の両方から到達できる() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let menu = &include_str!("../menu_bar.rs").replace("\r\n", "\n");
    let list = src
        .split("fn palette_builtin_cmds(&self) -> Vec<(String, String, String, Cmd)> {")
        .nth(1)
        .expect("パレットの一覧がある");
    let router = src
        .split("fn apply_cmd(&mut self, cmd: Cmd, ctx: &egui::Context) {")
        .nth(1)
        .expect("ディスパッチがある");
    let keys = src
        .split("fn handle_shortcuts(&mut self, ctx: &egui::Context)")
        .nth(1)
        .expect("ショートカット処理がある");
    for (cmd, action) in [
        ("Cmd::ZoomIn", "BindAction::ZoomIn"),
        ("Cmd::ZoomOut", "BindAction::ZoomOut"),
        ("Cmd::ZoomReset", "BindAction::ZoomReset"),
        ("Cmd::FileZoomIn", "BindAction::FileZoomIn"),
        ("Cmd::FileZoomOut", "BindAction::FileZoomOut"),
        ("Cmd::FileZoomReset", "BindAction::FileZoomReset"),
    ] {
        assert!(list.contains(cmd), "{cmd} がコマンドパレットに無い");
        assert!(router.contains(cmd), "{cmd} のディスパッチが無い");
        assert!(menu.contains(cmd), "{cmd} が表示メニューに無い");
        assert!(keys.contains(action), "{action} のキーバインド処理が無い");
    }
}

/// ⌘⌥- は「ファイル単位」だけに効き、「画面全体」へは流れない。
///
/// egui の `matches_logically` は余分な修飾キーを許すので、⌘⌥- は
/// ⌘- のパターンにも一致する。⌥ 付きを先に消費していないと、
/// ファイル単位のズームのつもりで画面全体が動く (しかも 2 つ同時に動く)。
/// ここは順序が仕様なので、実際のイベントで固定する。
#[test]
fn ファイル単位のズームキーが画面全体へ漏れない() {
    use egui::{Key, Modifiers};
    let keys = Keybinds::default();
    // 前提: egui の照合は余分な修飾キーを許す (許さなくなったら順序依存が消える)
    assert!(
        Modifiers::COMMAND
            .plus(Modifiers::ALT)
            .matches_logically(Modifiers::COMMAND),
        "egui の修飾キー照合が変わった (この順序依存の前提が崩れている)"
    );

    for (file_act, ui_act, key) in [
        (BindAction::FileZoomIn, BindAction::ZoomIn, Key::Plus),
        (BindAction::FileZoomOut, BindAction::ZoomOut, Key::Minus),
        (BindAction::FileZoomReset, BindAction::ZoomReset, Key::Num0),
    ] {
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        // ファイル単位ズームの修飾キーは OS で違う (非 mac は ⇧ を足して
        // 「戻る」との衝突を避ける)。ここで直書きすると非 mac で外れる。
        let mods = crate::keybinds::file_zoom_mods_for_test();
        input.events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: mods,
        });
        input.modifiers = mods;
        let (file_sc, ui_sc) = (keys.get(file_act), keys.get(ui_act));
        let (mut hit_file, mut hit_ui) = (false, false);
        let _ = ctx.run(input, |ctx| {
            // handle_shortcuts と同じ順序 (⌥ 付きが先)
            // handle_shortcuts と同じ互換経路・同じ順序 (⌥ 付きが先)
            hit_file = ctx.input_mut(|i| crate::keybinds::consume_shortcut_compat(i, file_sc));
            hit_ui = ctx.input_mut(|i| crate::keybinds::consume_shortcut_compat(i, ui_sc));
        });
        assert!(hit_file, "{file_act:?} が ⌘⌥ の打鍵を拾えていない");
        assert!(!hit_ui, "{ui_act:?} まで発火している (画面全体へ漏れた)");
    }
}

/// ファイル単位の倍率は **エディタの論理フォントサイズにだけ** 掛ける。
///
/// 画面全体のズームは egui が `pixels_per_point` 側で掛けるので、
/// ここでも掛けると倍率が二乗になる (150% × 150% = 225% に見える)。
#[test]
fn ファイル単位のズームは画面全体と二重に掛からない() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn editor_font_pt(&self) -> f32 {")
        .nth(1)
        .expect("editor_font_pt がある");
    let head = &body[..body.find("\n    }\n").unwrap_or(body.len())];
    assert!(
        head.contains("self.file_zoom()"),
        "ファイル倍率を掛けていない"
    );
    assert!(
        !head.contains("ui_zoom"),
        "画面全体の倍率まで掛けている (二重適用): {head}"
    );
    // エディタ本文はこの関数を通す (cfg を直に読むと倍率が抜ける)
    let editor = src
        .split("fn code_editor_ui(&mut self, ui: &mut egui::Ui) {")
        .nth(1)
        .expect("code_editor_ui がある");
    let head = &editor[..editor
        .find("self.last_row_h = row_h;")
        .unwrap_or(editor.len())];
    assert!(
        head.contains("self.editor_font_pt()"),
        "本文のフォントサイズがファイル倍率を通っていない"
    );
}

/// 画面全体のズームは egui の `zoom_factor` を動かし、UI 全体の
/// `pixels_per_point` に乗る。テーマ側の丸め直しもそれに追随する。
#[test]
fn 画面全体のズームがピクセル密度と文字サイズへ届く() {
    let ctx = egui::Context::default();
    super::install_fonts(&ctx);
    crate::theme::apply(&ctx, &crate::theme::by_name("zaivern-dark"));
    let _ = ctx.run(Default::default(), |_| {});
    let native = ctx.native_pixels_per_point().unwrap_or(1.0);

    for z in [0.5_f32, 1.0, 1.25, 2.0, 3.0] {
        super::apply_ui_zoom(&ctx, z);
        // set_zoom_factor はパス末尾で効くので、1 フレーム回してから見る
        let _ = ctx.run(Default::default(), |_| {});
        assert_eq!(ctx.zoom_factor(), z, "zoom_factor が入っていない");
        assert!(
            (ctx.pixels_per_point() - native * z).abs() < 1e-3,
            "pixels_per_point が倍率に追随していない ({z})"
        );
        // 文字サイズは論理ポイントのまま (拡大は ppp 側が担う) だが、
        // 物理ピクセル整数への丸めは新しい ppp でやり直されている
        let _ = ctx.run(Default::default(), |_| {});
        let ppp = ctx.pixels_per_point();
        let body = ctx.style().text_styles[&egui::TextStyle::Body].size;
        assert!(
            (body - crate::theme::snap_font_size(crate::theme::BASE_BODY_SIZE, ppp)).abs() < 1e-4,
            "倍率 {z} で文字サイズが丸め直されていない (body={body}, ppp={ppp})"
        );
    }
}

/// egui 内蔵のキーボードズームは切っておく。
///
/// 残すと `Config::ui_zoom` と egui が同じ `zoom_factor` を毎フレーム
/// 奪い合い、「⌘+ が効かない / 倍率が保存されない」という形で壊れる。
/// ズームの所有者は 1 つに絞る (設計原則: 状態の持ち主を 1 つにする)。
#[test]
fn egui内蔵のキーボードズームは無効化されている() {
    let ctx = egui::Context::default();
    assert!(
        ctx.options(|o| o.zoom_with_keyboard),
        "egui の既定が変わった (この対策の前提が崩れている)"
    );
    super::apply_ui_zoom(&ctx, 1.0);
    assert!(
        !ctx.options(|o| o.zoom_with_keyboard),
        "内蔵ズームを切っていない"
    );
}

/// 壊れた設定値でも UI が潰れない。
///
/// `ui_zoom = 0` を素通しすると全部が 0 ピクセルになり、設定を戻す口ごと
/// 消えて操作不能になる。ここは必ずクランプを通す。
#[test]
fn 壊れたズーム値でも表示が潰れない() {
    let ctx = egui::Context::default();
    let _ = ctx.run(Default::default(), |_| {});
    for bad in [0.0_f32, -3.0, 1e9, f32::NAN, f32::INFINITY] {
        super::apply_ui_zoom(&ctx, bad);
        let _ = ctx.run(Default::default(), |_| {});
        let z = ctx.zoom_factor();
        assert!(
            (crate::zoom::MIN..=crate::zoom::MAX).contains(&z),
            "{bad} → {z} が範囲外"
        );
        assert!(ctx.pixels_per_point() > 0.0);
    }
}
