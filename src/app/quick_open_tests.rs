use super::*;

/// ルート直下に `rel` があるものとして索引の 1 件を作る。
fn indexed(root: &Path, rel: &str) -> IndexedFile {
    IndexedFile {
        abs: root.join(rel),
        rel: rel.to_string(),
        label: rel.to_string(),
    }
}

fn labels(items: &[Item]) -> Vec<String> {
    let p = crate::palette::Palette::new();
    let res = p.results(items.to_vec(), &[]);
    res.items.iter().map(|i| i.label.clone()).collect()
}

#[test]
fn 空クエリでは最近開いたファイルが先頭に並ぶ() {
    let root = std::env::temp_dir().join("zv-quick-open");
    let index: Vec<IndexedFile> = ["a.rs", "b.rs", "c.rs", "d.rs"]
        .iter()
        .map(|r| indexed(&root, r))
        .collect();
    // 最近開いた順: d → b (残りは未オープン)
    let recent = vec![
        root.join("d.rs").display().to_string(),
        root.join("b.rs").display().to_string(),
    ];
    let items = file_mode_items(&index, &recent, None, "");
    assert_eq!(
        labels(&items),
        vec!["d.rs", "b.rs", "a.rs", "c.rs"],
        "最近順のあとはアルファベット順で続く"
    );
}

#[test]
fn 開いているファイルは先頭に来ず直前のファイルへ戻れる() {
    let root = std::env::temp_dir().join("zv-quick-open");
    let index: Vec<IndexedFile> = ["a.rs", "b.rs", "z.rs"]
        .iter()
        .map(|r| indexed(&root, r))
        .collect();
    // いま開いているのは z.rs、その前が b.rs
    let recent = vec![
        root.join("z.rs").display().to_string(),
        root.join("b.rs").display().to_string(),
    ];
    let active = root.join("z.rs");
    let items = file_mode_items(&index, &recent, Some(&active), "");
    let l = labels(&items);
    assert_eq!(l[0], "b.rs", "Enter で直前のファイルへ戻れない: {l:?}");
    assert_ne!(l[0], "z.rs", "開いているファイルが先頭に居座っている");
}

#[test]
fn 入力を始めると一致の質が最近順より優先される() {
    let root = std::env::temp_dir().join("zv-quick-open");
    let index: Vec<IndexedFile> = ["recent_only.rs", "zebra.rs"]
        .iter()
        .map(|r| indexed(&root, r))
        .collect();
    let recent = vec![root.join("recent_only.rs").display().to_string()];
    // "zebra" は前方一致 (TIER_PREFIX)。最近順の加点では追い越せない。
    let items = file_mode_items(&index, &recent, None, "zebra");
    assert_eq!(labels(&items)[0], "zebra.rs");
}

#[test]
fn ファイル名に行番号を付けると位置つきで開く候補になる() {
    let root = std::env::temp_dir().join("zv-quick-open");
    let index = vec![indexed(&root, "main.rs")];
    for (q, want) in [("main.rs:42", (41, 0)), ("main.rs:42:5", (41, 4))] {
        let items = file_mode_items(&index, &[], None, q);
        assert_eq!(items.len(), 1, "q={q}");
        match &items[0].action {
            Action::OpenFileAt(p, line, col) => {
                assert_eq!(p, &root.join("main.rs"), "q={q}");
                assert_eq!((*line, *col), want, "q={q}");
            }
            _ => panic!("位置つきで開く候補になっていない: {q}"),
        }
        assert!(items[0].detail.contains("42"), "行番号が見えない: {q}");
    }
    // 行指定が無ければ従来どおり
    let items = file_mode_items(&index, &[], None, "main");
    assert!(matches!(items[0].action, Action::OpenFile(_)));
}

#[test]
fn 行指定として読めないコロンは名前の一部として扱う() {
    // Windows のドライブレターや `foo:bar` を行ジャンプにしない
    for q in ["C:/work/main.rs", "C:\\work\\main.rs", "foo:bar", "a::b"] {
        assert_eq!(split_path_goto(q), (q, None), "q={q}");
    }
    assert_eq!(split_path_goto("main.rs:12"), ("main.rs", Some((11, 0))));
    assert_eq!(split_path_goto("main.rs:12:3"), ("main.rs", Some((11, 2))));
    // 先頭の `:` は行ジャンプモードの担当なのでここでは割らない
    assert_eq!(split_path_goto(":12"), (":12", None));
}

#[test]
fn 範囲外の行番号はクランプされてパニックしない() {
    let text = "1 行目\n2 行目\n3 行目";
    // parse_goto は 1 起点 → 0 起点。0 行目・巨大値も受け付ける
    for (input, want_line) in [
        ("0", 0usize),
        ("1", 0),
        ("3", 2),
        ("999999", 999_998),
        ("18446744073709551615", usize::MAX - 1),
    ] {
        let (line, col) = editor_ops::parse_goto(input).expect(input);
        assert_eq!(line, want_line, "input={input}");
        // 本文末尾へ丸まるだけで、添字は必ず本文の範囲に収まる
        let ch = editor_ops::char_index_at(text, line, col);
        assert!(ch <= text.chars().count(), "input={input} ch={ch}");
    }
    // 負の値・数字以外は候補にしない (パースが弾く)
    for bad in ["-1", "-1:2", "abc", "", " ", "1.5"] {
        assert_eq!(editor_ops::parse_goto(bad), None, "bad={bad}");
    }
    // 桁が本文より大きくても丸まる
    let ch = editor_ops::char_index_at(text, 0, usize::MAX);
    assert_eq!(ch, "1 行目".chars().count());
}

/// 行ジャンプの入口は 2 つ (パレットの `:` モードと ⌃G の小窓)。
/// **どちらも** `palette::fold_goto` を通してから `parse_goto` を呼ぶこと。
/// 片方だけ直すと「パレットでは飛べるのに小窓では飛べない」が残る。
#[test]
fn 行ジャンプの入口はどちらも全角を畳んでから読む() {
    let src = crate::app::SRC.replace("\r\n", "\n");
    let direct = src.matches("editor_ops::parse_goto(").count();
    let folded = src
        .matches("editor_ops::parse_goto(&crate::palette::fold_goto(")
        .count()
        + src.matches("let q = crate::palette::fold_goto(").count();
    // 行ジャンプの 2 経路 + `split_path_goto` (パスの `:12` 用。ここは
    // ファイル名の一部なので畳んではいけない) + このテストと隣のテストの参照。
    assert!(
        folded >= 2,
        "全角を畳んでいる行ジャンプ経路が {folded} 本しかない (direct={direct})"
    );
    assert!(
        src.contains("editor_ops::parse_goto(&crate::palette::fold_goto(&self.goto_input))"),
        "⌃G の小窓が全角数字を畳んでいない"
    );
}

#[test]
fn 日本語と絵文字を含むパスでもバイト境界を割らない() {
    let root = std::env::temp_dir().join("zv-quick-open");
    let names = [
        "日本語のファイル.rs",
        "絵文字🎨入り/設定🚀.toml",
        "🇯🇵/国旗.md",
        "混在_ひらがな_カタカナ_漢字.txt",
    ];
    let index: Vec<IndexedFile> = names.iter().map(|r| indexed(&root, r)).collect();
    // 部分一致・マルチバイト境界をまたぐクエリでも落ちない
    for q in ["日本語", "🎨", "設定", "国旗", "ひらがな", "🇯🇵", "な"] {
        let items = file_mode_items(&index, &[], None, q);
        for it in &items {
            assert!(!it.label.is_empty(), "q={q}");
            // ラベルはファイル名部分 (`/` の右側) を正しく切り出す
            assert!(!it.label.contains('/'), "q={q} label={}", it.label);
        }
    }
    // 行指定つきでも同じ
    let items = file_mode_items(&index, &[], None, "絵文字🎨入り/設定🚀.toml:7");
    assert_eq!(items.len(), 1);
    assert!(matches!(items[0].action, Action::OpenFileAt(_, 6, 0)));
    assert_eq!(items[0].label, "設定🚀.toml");
}

/// 矢印キーは**選択を動かすだけ**。開くのは Enter とクリックだけで、
/// 選択が動いたついでに裏でファイルを開いてタブを増やしたりしない。
#[test]
fn 矢印キーはパレットを閉じずファイルも開かない() {
    let src = crate::app::SRC.replace("\r\n", "\n");
    let after = src
        .split("fn palette_ui(&mut self, ctx: &egui::Context) {")
        .nth(1)
        .expect("palette_ui が見つからない");
    let body = &after[..crate::app::method_end(after)];
    // 上下キーの扱いは「選択位置の更新」1 行だけ
    assert_eq!(
        body.matches("results.step(self.palette.selected, down, up)")
            .count(),
        1,
        "上下キーの扱いが 1 か所でない"
    );
    // 閉じるのは Esc / Enter / クリックだけ (down/up では close にしない)
    assert!(
        !body.contains("if down") && !body.contains("if up"),
        "上下キーで別の分岐に入っている"
    );
    // 選択が動いただけでファイルを開かない (タブを増やさない)
    assert!(
        !body.contains("self.open_path("),
        "パレットの描画中にファイルを開いている"
    );
    // 実行は「Enter」と「クリックの戻り値」の 2 経路のみ
    assert_eq!(
        body.matches("execute = Some").count(),
        2,
        "実行の起点が 2 経路ではない"
    );
}

#[test]
fn 最近順の加点は索引の末尾でも正の値のまま() {
    // `recent::MAX_RECENT` 件ぶん下がっても 0 を割らない
    // (割ると「最近開いた」が未オープンより下に沈む)
    let last = RECENT_FILE_BONUS - (crate::recent::MAX_RECENT as i32 - 1) * RECENT_FILE_STEP;
    assert!(last > 0, "最近順の加点が尽きている: {last}");
    // 一致の段 (TIER_SUBSTR = 30_000) は超えない
    assert!(RECENT_FILE_BONUS < 30_000, "最近順が一致の質を追い越す");
}
