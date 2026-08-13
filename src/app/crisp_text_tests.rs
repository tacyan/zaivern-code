use super::*;

/// egui 同梱の「縮小された」絵文字フォント。
/// `FontTweak.scale` が 0.81 / 0.90 なので、ここで `✓ ✕ ▸` や罫線が
/// 解決されると本文の中で 1 文字だけ小さく沈む。
const SHRUNKEN: [&str; 2] = ["NotoEmoji-Regular", "emoji-icon-font"];

fn dummy_font(dir: &Path, name: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, b"not-a-real-font").expect("書けるはず");
    p.to_string_lossy().into_owned()
}

/// **回帰の要**: 自前のフォールバックは主フォントの直後 —
/// egui 同梱の縮小絵文字フォントより**前**に入る。
#[test]
fn fallback_faces_are_inserted_before_the_shrunken_emoji_fonts() {
    let dir = crate::test_util::unique_temp_dir("zaivern-font-test", "order");
    let cjk = dummy_font(&dir, "cjk.ttf");
    let s1 = dummy_font(&dir, "sym1.ttf");
    let s2 = dummy_font(&dir, "sym2.ttf");

    let mut fonts = egui::FontDefinitions::default();
    // 素の egui は縮小絵文字フォントを列に持っている (前提が崩れたら気付く)
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = &fonts.families[&fam];
        assert!(
            SHRUNKEN.iter().any(|n| list.iter().any(|x| x == n)),
            "{fam:?} に同梱絵文字フォントが居ない: {list:?}"
        );
    }

    let loaded = push_fallback_font(&mut fonts, "cjk", &[cjk.as_str()]);
    assert_eq!(loaded, Some(cjk.as_str()));
    let n = push_fallback_fonts_all(
        &mut fonts,
        "symbols",
        &[s1.as_str(), s2.as_str()],
        loaded,
        1 + usize::from(loaded.is_some()),
    );
    assert_eq!(n, 2);

    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = &fonts.families[&fam];
        assert_eq!(
            list[1], "cjk",
            "{fam:?}: CJK が主フォントの直後にない {list:?}"
        );
        assert_eq!(list[2], "symbols0", "{fam:?}: 記号の順序が崩れた {list:?}");
        assert_eq!(list[3], "symbols1", "{fam:?}: 記号の順序が崩れた {list:?}");
        for shrunken in SHRUNKEN {
            if let Some(at) = list.iter().position(|x| x == shrunken) {
                assert!(at > 3, "{fam:?}: {shrunken} が自前フォントより前 {list:?}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// 候補が 1 つも読めなくても列を壊さない (フォントの無い環境で起動不能にしない)。
#[test]
fn missing_fallback_candidates_leave_the_list_intact() {
    let mut fonts = egui::FontDefinitions::default();
    let before = fonts.families.clone();
    let dir = crate::test_util::unique_temp_dir("zaivern-font-test", "missing");
    let nope = dir.join("いない.ttf").to_string_lossy().into_owned();
    assert_eq!(
        push_fallback_font(&mut fonts, "cjk", &[nope.as_str()]),
        None
    );
    assert_eq!(
        push_fallback_fonts_all(&mut fonts, "symbols", &[nope.as_str()], None, 1),
        0
    );
    assert_eq!(fonts.families, before);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Windows では本文 (Proportional) の主フェイスを OS の日本語フェイスに
/// する。ラテンと日本語が別フェイスだと、epaint はフェイスごとの ascent で
/// 置くため同じ行に 2 本のベースラインができて上下にずれる。
/// 実機でしか走らない分岐なので、ソースで固定する。
#[test]
fn windows_body_face_is_the_os_japanese_face() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn install_fonts(ctx: &egui::Context) {")
        .nth(1)
        .expect("install_fonts があるはず");
    let head = &body[..body.find("\n}\n").unwrap_or(body.len())];
    assert!(
        head.contains("#[cfg(target_os = \"windows\")]"),
        "Windows 分岐が消えた"
    );
    assert!(
        head.contains("list.insert(0, \"cjk\".to_owned());"),
        "Proportional の先頭へ CJK を回していない"
    );
    assert!(
        !head.contains("FontFamily::Monospace)"),
        "Monospace まで入れ替えている (桁の等幅性が壊れる)"
    );
}

/// エディタの行高も物理ピクセルの整数へ揃える (端末と同じ理由)。
/// 行高が小数だと行ごとに丸めがずれ、ガター番号と本文が 1px 単位で踊る。
#[test]
fn editor_row_height_is_snapped() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn code_editor_ui(&mut self, ui: &mut egui::Ui) {")
        .nth(1)
        .expect("code_editor_ui があるはず");
    let head = &body[..body.find("self.last_row_h = row_h;").unwrap_or(body.len())];
    assert!(
        head.contains("crate::theme::snap_font_size("),
        "エディタのフォントサイズがスナップされていない"
    );
    assert!(
        head.contains("crate::theme::snap_len("),
        "エディタの行高がスナップされていない"
    );

    // 実際の丸め結果が物理ピクセル整数になることも確かめる。
    for ppp in [1.0_f32, 1.25, 1.5, 2.0] {
        for size in [11.0_f32, 12.5, 14.0] {
            let s = crate::theme::snap_font_size(size, ppp);
            let px = s * ppp;
            assert!((px - px.round()).abs() < 1e-3, "{size} @ppp {ppp} → {px}px");
        }
    }
}

/// 端末の描画とセル寸法の計算は 1 か所 (`terminal::cell_metrics`) に
/// まとめる。ここが分かれると「描いた矩形と PTY のグリッド」がずれる。
#[test]
fn terminal_cell_metrics_have_one_source() {
    let src = &include_str!("../terminal.rs").replace("\r\n", "\n");
    assert_eq!(
        src.matches("f.glyph_width(&font_id, 'M')").count(),
        1,
        "セル幅の計算が 2 か所以上ある"
    );
    assert!(
        src.contains("let (font_id, cell_w, cell_h) = cell_metrics(ui, font_size);"),
        "draw が cell_metrics を通っていない"
    );
}
