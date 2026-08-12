use super::{
    filter_problems, group_problems, problem_counts, ProblemItem, ProblemRow, ProblemsFilter,
};
use std::collections::HashSet;
use std::path::PathBuf;

/// テスト用の診断 1 件。パスは相対名だけで実在させない
/// (問題パネルは実ファイルを読まないので触る必要が無い)。
fn item(path: &str, line: usize, col: usize, sev: u8, msg: &str) -> ProblemItem {
    let p = PathBuf::from(path);
    ProblemItem {
        title: p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path: p,
        line,
        col,
        severity: sev,
        message: msg.to_string(),
        can_fix: true,
        open: false,
    }
}

fn rows(items: Vec<ProblemItem>) -> Vec<ProblemRow> {
    group_problems(items, &HashSet::new())
}

#[test]
fn 零件なら行も零件() {
    assert!(rows(Vec::new()).is_empty());
    assert_eq!(problem_counts(&[]), [0, 0, 0, 0]);
}

#[test]
fn 一件なら見出しと本文の二行になる() {
    let got = rows(vec![item("src/a.rs", 0, 0, 1, "boom")]);
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(matches!(&got[0], ProblemRow::Header { count: 1, .. }));
    assert!(matches!(&got[1], ProblemRow::Item(_)));
}

#[test]
fn 同じファイルの複数診断は一つの見出しにまとまる() {
    let got = rows(vec![
        item("src/a.rs", 5, 1, 2, "warn"),
        item("src/a.rs", 1, 2, 1, "err"),
        item("src/b.rs", 0, 0, 2, "warn"),
    ]);
    let headers: Vec<_> = got
        .iter()
        .filter_map(|r| match r {
            ProblemRow::Header {
                title,
                count,
                worst,
                ..
            } => Some((title.clone(), *count, *worst)),
            ProblemRow::Item(_) => None,
        })
        .collect();
    assert_eq!(
        headers,
        vec![("a.rs".into(), 2usize, 1u8), ("b.rs".into(), 1, 2)],
        "エラーを含むファイルが先、件数と最悪 severity が出る"
    );
    // ファイル内は行順
    let lines: Vec<usize> = got
        .iter()
        .filter_map(|r| match r {
            ProblemRow::Item(i) if i.title == "a.rs" => Some(i.line),
            _ => None,
        })
        .collect();
    assert_eq!(lines, vec![1, 5]);
}

#[test]
fn 折り畳んだファイルは見出しだけ残る() {
    let items = vec![
        item("src/a.rs", 0, 0, 1, "err"),
        item("src/b.rs", 0, 0, 1, "err"),
    ];
    let mut collapsed = HashSet::new();
    collapsed.insert(PathBuf::from("src/a.rs"));
    let got = group_problems(items, &collapsed);
    assert_eq!(got.len(), 3, "見出し 2 + 畳んでいない方の本文 1: {got:?}");
}

#[test]
fn severityのトグルで絞り込める() {
    let items = vec![
        item("src/a.rs", 0, 0, 1, "err"),
        item("src/a.rs", 1, 0, 2, "warn"),
        item("src/a.rs", 2, 0, 3, "info"),
        item("src/a.rs", 3, 0, 4, "hint"),
    ];
    assert_eq!(problem_counts(&items), [1, 1, 1, 1]);
    let mut f = ProblemsFilter::default();
    assert_eq!(filter_problems(&items, &f).len(), 4);
    f.sev = [true, false, false, false];
    let only_err = filter_problems(&items, &f);
    assert_eq!(only_err.len(), 1);
    assert_eq!(only_err[0].severity, 1);
    f.sev = [false; 4];
    assert!(filter_problems(&items, &f).is_empty());
}

#[test]
fn テキスト絞り込みはファイル名とメッセージの両方に効く() {
    let items = vec![
        item("src/alpha.rs", 0, 0, 1, "unused variable"),
        item("src/beta.rs", 0, 0, 1, "missing semicolon"),
    ];
    let hit = |q: &str| -> Vec<String> {
        let f = ProblemsFilter {
            sev: [true; 4],
            text: q.to_string(),
        };
        filter_problems(&items, &f)
            .into_iter()
            .map(|i| i.title)
            .collect()
    };
    assert_eq!(hit("alpha"), vec!["alpha.rs".to_string()], "ファイル名");
    assert_eq!(hit("semicolon"), vec!["beta.rs".to_string()], "メッセージ");
    assert_eq!(hit("src/beta"), vec!["beta.rs".to_string()], "パス");
    assert_eq!(hit(""), vec!["alpha.rs".to_string(), "beta.rs".to_string()]);
    assert!(hit("ぜんぜん一致しない語").is_empty());
}

#[test]
fn 絞り込みで零件になったら行も零件() {
    let items = vec![item("src/a.rs", 0, 0, 1, "err")];
    let f = ProblemsFilter {
        sev: [true; 4],
        text: "zzzz".into(),
    };
    assert!(rows(filter_problems(&items, &f)).is_empty());
}

#[test]
fn 大量の診断でも行数が線形に収まる() {
    // 1000 件 = 100 ファイル × 10 件。見出し 100 + 本文 1000 = 1100 行。
    // `ScrollArea::show_rows` は見えている分しか描かないので、
    // ここで押さえるのは「行の作り方が破綻しない」ことだけ。
    let mut items = Vec::new();
    for f in 0..100 {
        for l in 0..10 {
            items.push(item(
                &format!("src/f{f:03}.rs"),
                l,
                0,
                1 + (l % 4) as u8,
                "x",
            ));
        }
    }
    assert_eq!(items.len(), 1000);
    let got = rows(items.clone());
    assert_eq!(got.len(), 1100, "見出し 100 + 本文 1000");
    // 絞り込みも 1000 件で壊れない
    let f = ProblemsFilter {
        sev: [true, false, false, false],
        text: "f001".into(),
    };
    let narrowed = filter_problems(&items, &f);
    assert!(!narrowed.is_empty() && narrowed.len() < 1000);
    assert!(narrowed.iter().all(|i| i.severity == 1));
}

/// 0 件のときの空状態は **窓の中央に 1 枚**で、はみ出さないこと。
///
/// 極端な窓サイズ (低い窓・広い窓) でカードが下へ突き抜けると、
/// 「何も無いのに何も見えない」パネルになる。
#[test]
fn 空状態のカードは窓の中に収まる() {
    use eframe::egui::{pos2, vec2, Rect};
    let ctx = egui::Context::default();
    let theme = crate::theme::by_name("zaivern-dark");
    for screen in [vec2(900.0, 700.0), vec2(1200.0, 300.0), vec2(520.0, 240.0)] {
        let mut card = Rect::NOTHING;
        let mut inner = Rect::NOTHING;
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), screen)),
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            let mut open = true;
            egui::Window::new("問題")
                .open(&mut open)
                .default_size([620.0, 340.0])
                .show(ctx, |ui| {
                    inner = ui.clip_rect();
                    let avail = ui.available_rect_before_wrap().intersect(ui.clip_rect());
                    card = crate::panels::empty_card(avail, 0).card;
                    super::problems_empty_card(ui, &theme, "問題はありません", "対象なし");
                });
        });
        assert!(
            card.width() > 0.0 && card.height() > 0.0 && card.width().is_finite(),
            "{screen:?}: カードの寸法が壊れた {card:?}"
        );
        assert!(
            inner.contains_rect(card),
            "{screen:?}: カード {card:?} が窓 {inner:?} をはみ出した"
        );
    }
}

#[test]
fn 開いていないファイルの診断も行になる() {
    // 問題パネルはバッファではなく LSP の診断表から作るので、
    // `open == false` でも必ず一覧に出る。
    let mut a = item("src/closed.rs", 3, 7, 1, "err");
    a.open = false;
    let got = rows(vec![a]);
    assert_eq!(got.len(), 2, "{got:?}");
    match &got[1] {
        ProblemRow::Item(i) => {
            assert!(!i.open);
            // 行だけでなく桁も保つ (クリックで行と桁へ飛ぶため)
            assert_eq!((i.line, i.col), (3, 7));
        }
        ProblemRow::Header { .. } => panic!("2 行目は本文のはず"),
    }
}
