use eframe::egui::{self, Color32};

#[derive(Clone)]
pub struct Theme {
    pub name: String,
    pub label: String,
    pub dark: bool,
    /// Editor / central background
    pub bg: Color32,
    /// Side / top panels
    pub panel: Color32,
    /// Tab bar, inactive widgets
    pub panel_alt: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub border: Color32,
    pub term_bg: Color32,
    pub term_fg: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub err: Color32,
    pub ansi: [Color32; 16],
    pub syntect_theme: String,
}

const fn c(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

fn zaivern_dark() -> Theme {
    Theme {
        name: "zaivern-dark".into(),
        label: "Zaivern Dark".into(),
        dark: true,
        bg: c(0x0b, 0x0e, 0x14),
        panel: c(0x11, 0x15, 0x1f),
        panel_alt: c(0x0e, 0x12, 0x1b),
        accent: c(0x8b, 0x7c, 0xf6),
        accent_soft: c(0x2a, 0x2f, 0x45),
        text: c(0xe6, 0xe9, 0xf2),
        text_dim: c(0x8a, 0x91, 0xa8),
        border: c(0x1e, 0x24, 0x33),
        term_bg: c(0x0a, 0x0d, 0x13),
        term_fg: c(0xc8, 0xce, 0xdc),
        ok: c(0x4a, 0xde, 0x80),
        warn: c(0xfb, 0xbf, 0x24),
        err: c(0xf8, 0x71, 0x71),
        ansi: [
            c(0x15, 0x16, 0x1e),
            c(0xf7, 0x76, 0x8e),
            c(0x9e, 0xce, 0x6a),
            c(0xe0, 0xaf, 0x68),
            c(0x7a, 0xa2, 0xf7),
            c(0xbb, 0x9a, 0xf7),
            c(0x7d, 0xcf, 0xff),
            c(0xa9, 0xb1, 0xd6),
            c(0x41, 0x48, 0x68),
            c(0xf7, 0x76, 0x8e),
            c(0x9e, 0xce, 0x6a),
            c(0xe0, 0xaf, 0x68),
            c(0x7a, 0xa2, 0xf7),
            c(0xbb, 0x9a, 0xf7),
            c(0x7d, 0xcf, 0xff),
            c(0xc0, 0xca, 0xf5),
        ],
        syntect_theme: "base16-ocean.dark".into(),
    }
}

fn zaivern_midnight() -> Theme {
    Theme {
        name: "zaivern-midnight".into(),
        label: "Zaivern Midnight".into(),
        dark: true,
        bg: c(0x13, 0x0f, 0x1d),
        panel: c(0x1a, 0x14, 0x28),
        panel_alt: c(0x16, 0x11, 0x22),
        accent: c(0xe8, 0x7b, 0xf8),
        accent_soft: c(0x39, 0x2a, 0x4e),
        text: c(0xf0, 0xea, 0xf8),
        text_dim: c(0x9d, 0x92, 0xb5),
        border: c(0x2c, 0x22, 0x40),
        term_bg: c(0x10, 0x0c, 0x19),
        term_fg: c(0xd8, 0xd0, 0xe8),
        ok: c(0x4a, 0xde, 0x80),
        warn: c(0xfb, 0xbf, 0x24),
        err: c(0xf8, 0x71, 0x71),
        ansi: [
            c(0x1e, 0x17, 0x2e),
            c(0xff, 0x75, 0x9c),
            c(0xa0, 0xe8, 0x7a),
            c(0xff, 0xc7, 0x77),
            c(0x91, 0xa7, 0xff),
            c(0xe8, 0x7b, 0xf8),
            c(0x89, 0xdd, 0xff),
            c(0xc0, 0xb7, 0xd8),
            c(0x4e, 0x41, 0x6b),
            c(0xff, 0x75, 0x9c),
            c(0xa0, 0xe8, 0x7a),
            c(0xff, 0xc7, 0x77),
            c(0x91, 0xa7, 0xff),
            c(0xe8, 0x7b, 0xf8),
            c(0x89, 0xdd, 0xff),
            c(0xe8, 0xe2, 0xf5),
        ],
        syntect_theme: "base16-mocha.dark".into(),
    }
}

fn zaivern_light() -> Theme {
    Theme {
        name: "zaivern-light".into(),
        label: "Zaivern Light".into(),
        dark: false,
        bg: c(0xfb, 0xfb, 0xf9),
        panel: c(0xf1, 0xf1, 0xed),
        panel_alt: c(0xe9, 0xe9, 0xe4),
        accent: c(0x6f, 0x5b, 0xd0),
        accent_soft: c(0xe4, 0xdf, 0xf7),
        text: c(0x24, 0x28, 0x33),
        text_dim: c(0x74, 0x7a, 0x8a),
        border: c(0xd8, 0xd8, 0xd2),
        term_bg: c(0xff, 0xff, 0xfe),
        term_fg: c(0x2c, 0x31, 0x3d),
        ok: c(0x16, 0xa3, 0x4a),
        warn: c(0xb4, 0x83, 0x06),
        err: c(0xdc, 0x26, 0x26),
        ansi: [
            c(0x3a, 0x3f, 0x4b),
            c(0xd2, 0x1f, 0x3c),
            c(0x2e, 0x7d, 0x32),
            c(0xa8, 0x6a, 0x00),
            c(0x1a, 0x56, 0xdb),
            c(0x8b, 0x33, 0xc7),
            c(0x00, 0x74, 0x8a),
            c(0x6b, 0x72, 0x80),
            c(0x8a, 0x91, 0x9e),
            c(0xd2, 0x1f, 0x3c),
            c(0x2e, 0x7d, 0x32),
            c(0xa8, 0x6a, 0x00),
            c(0x1a, 0x56, 0xdb),
            c(0x8b, 0x33, 0xc7),
            c(0x00, 0x74, 0x8a),
            c(0x24, 0x28, 0x33),
        ],
        syntect_theme: "InspiredGitHub".into(),
    }
}

// ─── 物理ピクセルグリッドへの整合 (端数 DPI 対策) ────────────────────
//
// egui は「論理サイズ × pixels_per_point」でグリフをラスタライズし、
// epaint 側で最終的に整数ピクセルへ丸める
// (epaint 0.29 `text/font.rs` FontImpl::new の `scale_in_pixels.round()`)。
// 丸めが入るということは、論理サイズが端数だと **要求したサイズと実際に
// 焼かれるサイズがズレる**。しかもその丸め方向はフォールバックフェイスごとに
// 違う (`scale_in_pixels` にフェイス固有の height_unscaled/units_per_em が
// 掛かるため)。
//
// macOS の pixels_per_point は 1.0 か 2.0 しか出ないので端数は生まれないが、
// Windows は 125% / 150% / 175% が既定値として普通に出る。
// 13.5pt × 1.25 = 16.875px のような値になり、スタイルごと・フェイスごとに
// 丸めがバラついて行高・余白・ステム幅が揃わず「文字がガタガタ」に見える。
//
// 対策は二段構え:
//   1. 基準サイズを **整数** にする (100% と 200% では丸めが一切起きない)。
//   2. 実行時の pixels_per_point で論理サイズを丸め直し、端数スケールでも
//      物理ピクセル整数へ着地させる (`snap_font_size`)。
//
// `pixels_per_point` / `zoom_factor` 自体は決して変更しない。
// あれを触ると UI 全体の大きさが変わってしまい、別の問題になる。

/// UI テキストの基準サイズ (論理ポイント)。
///
/// **整数のみを入れること。** 等倍 (100%) と 2 倍 (Retina / 200%) で
/// 物理ピクセルがそのまま整数になり、丸めが起きなくなる。
/// この不変条件は `base_text_styles_are_pixel_exact_at_1x_and_2x` が守る。
pub const BASE_BODY_SIZE: f32 = 14.0;
/// ボタンラベル。本文と揃える (別サイズにすると行高がボタンごとにブレる)。
pub const BASE_BUTTON_SIZE: f32 = 14.0;
/// 補助テキスト (ステータスバー・注釈)。
pub const BASE_SMALL_SIZE: f32 = 11.0;
/// 見出し。
pub const BASE_HEADING_SIZE: f32 = 18.0;
/// 等幅 (コード片・ターミナル以外の inline monospace)。
pub const BASE_MONOSPACE_SIZE: f32 = 13.0;

/// `Style::spacing` の基準値 (論理ポイント)。
/// スナップは必ず **この基準値から** 行う。一度スナップした値を再スナップすると
/// スケールを行き来したときに誤差が積み上がるため。
const BASE_ITEM_SPACING: [f32; 2] = [8.0, 6.0];
const BASE_BUTTON_PADDING: [f32; 2] = [10.0, 5.0];
const BASE_MENU_MARGIN: f32 = 8.0;
const BASE_INTERACT_SIZE: [f32; 2] = [40.0, 18.0];

/// テキストスタイル表 (スナップ前の基準サイズ)。
pub fn base_text_styles() -> [(egui::TextStyle, f32, egui::FontFamily); 5] {
    use egui::{FontFamily, TextStyle};
    [
        (TextStyle::Body, BASE_BODY_SIZE, FontFamily::Proportional),
        (
            TextStyle::Button,
            BASE_BUTTON_SIZE,
            FontFamily::Proportional,
        ),
        (TextStyle::Small, BASE_SMALL_SIZE, FontFamily::Proportional),
        (
            TextStyle::Heading,
            BASE_HEADING_SIZE,
            FontFamily::Proportional,
        ),
        (
            TextStyle::Monospace,
            BASE_MONOSPACE_SIZE,
            FontFamily::Monospace,
        ),
    ]
}

/// 論理サイズ `size_pt` を、`ppp` のもとで **物理ピクセル整数** に着地する
/// 論理サイズへ丸める。
///
/// 例: 13.5pt @ 1.25 → 16.875px → 17px → 13.6pt。
/// 見た目の変化は必ず 1 物理ピクセル未満 (`|返り値 - size_pt| <= 0.5/ppp`)。
///
/// 異常値 (NaN / 無限 / 非正) はフェイルソフトでそのまま返す。
/// ここで panic すると起動不能になるので、絶対に assert しない。
pub fn snap_font_size(size_pt: f32, ppp: f32) -> f32 {
    if !size_pt.is_finite() || size_pt <= 0.0 || !ppp.is_finite() || ppp <= 0.0 {
        return size_pt;
    }
    // 最低 1 物理ピクセルは確保する (0px はラスタライザが扱えない)。
    let px = (size_pt * ppp).round().max(1.0);
    px / ppp
}

/// 余白・寸法用のスナップ。フォントと違い 0 や負値をそのまま通す
/// (0 の余白は 0 のままでなければレイアウトが崩れる)。
pub fn snap_len(len_pt: f32, ppp: f32) -> f32 {
    if !len_pt.is_finite() || !ppp.is_finite() || ppp <= 0.0 {
        return len_pt;
    }
    (len_pt * ppp).round() / ppp
}

fn snap_vec2(v: [f32; 2], ppp: f32) -> egui::Vec2 {
    egui::vec2(snap_len(v[0], ppp), snap_len(v[1], ppp))
}

/// `ppp` に合わせてスナップ済みのテキストスタイル表を返す。
pub fn text_styles(ppp: f32) -> Vec<(egui::TextStyle, egui::FontId)> {
    base_text_styles()
        .into_iter()
        .map(|(ts, size, fam)| (ts, egui::FontId::new(snap_font_size(size, ppp), fam)))
        .collect()
}

/// テキストサイズと余白を `ppp` の物理ピクセルグリッドへ合わせる。
/// 基準値からの再計算なので何度呼んでも同じ結果になる (冪等・誤差蓄積なし)。
fn apply_pixel_snapping(style: &mut egui::Style, ppp: f32) {
    for (ts, font) in text_styles(ppp) {
        style.text_styles.insert(ts, font);
    }
    style.spacing.item_spacing = snap_vec2(BASE_ITEM_SPACING, ppp);
    style.spacing.button_padding = snap_vec2(BASE_BUTTON_PADDING, ppp);
    style.spacing.menu_margin = egui::Margin::same(snap_len(BASE_MENU_MARGIN, ppp));
    // ボタン等の最小寸法。端数スケールで 22.5px のような値になると
    // ウィジェットごとに 1px ズレて縦のリズムが崩れる。
    style.spacing.interact_size = snap_vec2(BASE_INTERACT_SIZE, ppp);
}

/// `ctx` に紐づけて覚えておく「最後にスナップした pixels_per_point」。
fn snapped_ppp_id() -> egui::Id {
    egui::Id::new("zaivern::theme::snapped_ppp")
}

/// 現在の `pixels_per_point` に追従してテキストサイズ／余白を丸め直す。
///
/// 変化が無ければ **何も書かずに false を返す**。毎フレーム呼ばれる前提の
/// 安さ (f32 比較 1 回) で、再描画要求も一切出さないので
/// アイドル時の CPU/GPU コストはゼロのまま。
///
/// 別 DPI のディスプレイへウィンドウを移した / OS の拡大率を変えた場合に
/// true を返して再適用する。
pub fn resync_pixel_snapping(ctx: &egui::Context) -> bool {
    let ppp = ctx.pixels_per_point();
    if !ppp.is_finite() || ppp <= 0.0 {
        return false;
    }
    let id = snapped_ppp_id();
    let last = ctx.data(|d| d.get_temp::<f32>(id));
    if last.is_some_and(|l| l == ppp) {
        return false;
    }
    ctx.data_mut(|d| d.insert_temp(id, ppp));
    // テキストサイズはテーマに依存しないので、両テーマ分まとめて直す。
    ctx.all_styles_mut(|s| apply_pixel_snapping(s, ppp));
    true
}

/// `resync_pixel_snapping` をフレーム先頭で走らせるフックを **一度だけ** 登録する。
///
/// `Context::on_begin_pass` は呼ぶたびにコールバックを積むので、
/// テーマ切替で `apply` が何度呼ばれても増えないようフラグで守る。
fn install_pixel_snap_plugin(ctx: &egui::Context) {
    let id = egui::Id::new("zaivern::theme::snap_plugin_installed");
    if ctx.data(|d| d.get_temp::<bool>(id).unwrap_or(false)) {
        return;
    }
    ctx.data_mut(|d| d.insert_temp(id, true));
    ctx.on_begin_pass(
        "zaivern_font_pixel_snap",
        std::sync::Arc::new(|ctx: &egui::Context| {
            resync_pixel_snapping(ctx);
        }),
    );
}

pub fn all() -> Vec<Theme> {
    vec![zaivern_dark(), zaivern_midnight(), zaivern_light()]
}

pub fn by_name(name: &str) -> Theme {
    all()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(zaivern_dark)
}

pub fn apply(ctx: &egui::Context, t: &Theme) {
    let mut style = (*ctx.style()).clone();
    let mut v = if t.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    v.panel_fill = t.panel;
    v.window_fill = t.panel;
    v.window_stroke = egui::Stroke::new(1.0_f32, t.border);
    v.extreme_bg_color = t.bg;
    v.faint_bg_color = t.panel_alt;
    v.hyperlink_color = t.accent;
    v.selection.bg_fill = t.accent.gamma_multiply(0.35);
    v.selection.stroke = egui::Stroke::new(1.0_f32, t.accent);

    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, t.border);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, t.text);
    v.widgets.inactive.bg_fill = t.panel_alt;
    v.widgets.inactive.weak_bg_fill = t.panel_alt;
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, t.text);
    v.widgets.hovered.bg_fill = t.accent_soft;
    v.widgets.hovered.weak_bg_fill = t.accent_soft;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, t.accent.gamma_multiply(0.6));
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, t.text);
    v.widgets.active.bg_fill = t.accent.gamma_multiply(0.5);
    v.widgets.active.weak_bg_fill = t.accent.gamma_multiply(0.4);
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, t.text);
    v.widgets.open.bg_fill = t.accent_soft;
    v.widgets.open.weak_bg_fill = t.accent_soft;
    v.widgets.open.fg_stroke = egui::Stroke::new(1.0_f32, t.text);

    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.rounding = egui::Rounding::same(6.0);
    }
    v.window_rounding = egui::Rounding::same(10.0);
    v.menu_rounding = egui::Rounding::same(8.0);

    style.visuals = v;

    // テキストサイズと余白は、いま表示しているディスプレイの
    // pixels_per_point で物理ピクセル整数へ丸める (Windows の 125%/150%/175%
    // といった端数スケールでの「ガタガタ」対策。詳細はファイル上部の解説)。
    //
    // なお `apply` は最初のフレームより前 (App::new) にも呼ばれ、その時点の
    // pixels_per_point はまだ既定値 1.0 でしかない。実際の DPI が判るのは
    // 最初の begin_pass 以降なので、下の `install_pixel_snap_plugin` で
    // フレーム先頭の追従フックも仕込んでおく。
    let ppp = ctx.pixels_per_point();
    apply_pixel_snapping(&mut style, ppp);

    // OSのライト/ダーク切替に追従させず、Zaivern のテーマを常に優先する。
    // (これを行わないと OS がライトモードのとき Visuals が毎フレーム
    //  ライトテーマで上書きされ、パネルが白く・文字が薄くなる)
    ctx.set_theme(if t.dark {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    });
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);

    // いま焼いたサイズがどの ppp のものかを記録し、追従フックを登録する。
    // (記録しておかないと次フレームで無駄に丸め直しが走る)
    ctx.data_mut(|d| d.insert_temp(snapped_ppp_id(), ppp));
    install_pixel_snap_plugin(ctx);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- all ----

    #[test]
    fn all_returns_three_builtin_themes_in_order() {
        let names: Vec<String> = all().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["zaivern-dark", "zaivern-midnight", "zaivern-light"]);
    }

    #[test]
    fn all_names_are_unique() {
        let themes = all();
        let mut names: Vec<&str> = themes.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), themes.len(), "duplicate theme name in all()");
    }

    #[test]
    fn all_labels_are_unique_and_non_empty() {
        let themes = all();
        let mut labels: Vec<&str> = themes.iter().map(|t| t.label.as_str()).collect();
        assert!(labels.iter().all(|l| !l.is_empty()));
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), themes.len(), "duplicate theme label in all()");
    }

    // ---- by_name ----

    #[test]
    fn by_name_resolves_every_theme_from_all() {
        for t in all() {
            let found = by_name(&t.name);
            assert_eq!(found.name, t.name);
            assert_eq!(found.label, t.label);
            assert_eq!(found.dark, t.dark);
            assert_eq!(found.bg, t.bg);
            assert_eq!(found.syntect_theme, t.syntect_theme);
        }
    }

    #[test]
    fn by_name_known_names_return_expected_theme() {
        assert_eq!(by_name("zaivern-dark").label, "Zaivern Dark");
        assert_eq!(by_name("zaivern-midnight").label, "Zaivern Midnight");
        assert_eq!(by_name("zaivern-light").label, "Zaivern Light");
    }

    #[test]
    fn by_name_unknown_falls_back_to_zaivern_dark() {
        assert_eq!(by_name("no-such-theme").name, "zaivern-dark");
    }

    #[test]
    fn by_name_empty_string_falls_back_to_zaivern_dark() {
        assert_eq!(by_name("").name, "zaivern-dark");
    }

    #[test]
    fn by_name_is_case_sensitive() {
        // 大文字違いは既知名に一致せず、フォールバック (zaivern-dark) になる。
        assert_eq!(by_name("Zaivern-Light").name, "zaivern-dark");
        assert_eq!(by_name("ZAIVERN-MIDNIGHT").name, "zaivern-dark");
    }

    // ---- テーマ構築関数 ----

    #[test]
    fn zaivern_dark_is_dark_with_expected_identity() {
        let t = zaivern_dark();
        assert_eq!(t.name, "zaivern-dark");
        assert_eq!(t.label, "Zaivern Dark");
        assert!(t.dark);
        assert_eq!(t.syntect_theme, "base16-ocean.dark");
    }

    #[test]
    fn zaivern_midnight_is_dark_with_expected_identity() {
        let t = zaivern_midnight();
        assert_eq!(t.name, "zaivern-midnight");
        assert_eq!(t.label, "Zaivern Midnight");
        assert!(t.dark);
        assert_eq!(t.syntect_theme, "base16-mocha.dark");
    }

    #[test]
    fn zaivern_light_is_the_only_light_theme() {
        let t = zaivern_light();
        assert_eq!(t.name, "zaivern-light");
        assert_eq!(t.label, "Zaivern Light");
        assert!(!t.dark);
        assert_eq!(t.syntect_theme, "InspiredGitHub");
        assert_eq!(all().iter().filter(|t| !t.dark).count(), 1);
    }

    #[test]
    fn every_theme_has_readable_contrast_pairs() {
        // 文字色と背景色が同一だと自明に壊れているので、その退行だけを検出する。
        for t in all() {
            assert_ne!(t.text, t.bg, "{}: text == bg", t.name);
            assert_ne!(t.term_fg, t.term_bg, "{}: term_fg == term_bg", t.name);
            assert_ne!(t.text, t.panel, "{}: text == panel", t.name);
            assert_ne!(t.accent, t.bg, "{}: accent == bg", t.name);
        }
    }

    // ---- 物理ピクセルグリッドへのスナップ ----

    /// Windows で実際に出る拡大率 (100/125/150/175%) と macOS の Retina (200%)、
    /// さらに「割り切れない」値の代表として 1.1 を混ぜる。
    const TEST_SCALES: [f32; 6] = [1.0, 1.25, 1.5, 1.75, 2.0, 1.1];

    /// 表に載っているサイズ + 設定から来る可能性のあるサイズ。
    fn probe_sizes() -> Vec<f32> {
        let mut v: Vec<f32> = base_text_styles().iter().map(|(_, s, _)| *s).collect();
        // config.rs の editor/terminal font size のクランプ範囲の端と、
        // 旧実装が使っていた端数サイズ 13.5 も通す。
        v.extend([7.0, 8.0, 10.0, 13.5, 15.0, 16.0, 21.0, 28.0, 32.0]);
        v
    }

    #[test]
    fn snap_font_size_lands_on_whole_physical_pixels() {
        for ppp in TEST_SCALES {
            for size in probe_sizes() {
                let snapped = snap_font_size(size, ppp);
                let px = snapped * ppp;
                assert!(
                    (px - px.round()).abs() < 1e-3,
                    "ppp={ppp} size={size}: {snapped}pt -> {px}px が整数でない"
                );
                assert!(px >= 1.0, "ppp={ppp} size={size}: {px}px が 1 未満");
            }
        }
    }

    #[test]
    fn snap_font_size_stays_within_half_a_physical_pixel() {
        // 見た目が変わってしまっては本末転倒なので、ズレは常に 1 物理ピクセル未満。
        for ppp in TEST_SCALES {
            for size in probe_sizes() {
                let snapped = snap_font_size(size, ppp);
                let tolerance = 0.5 / ppp + 1e-4;
                assert!(
                    (snapped - size).abs() <= tolerance,
                    "ppp={ppp} size={size}: {snapped}pt はズレすぎ (許容 {tolerance})"
                );
            }
        }
    }

    #[test]
    fn snap_font_size_is_idempotent() {
        // 同じ ppp で二度掛けても動かない = 毎フレーム呼んでも安全。
        for ppp in TEST_SCALES {
            for size in probe_sizes() {
                let once = snap_font_size(size, ppp);
                let twice = snap_font_size(once, ppp);
                assert!(
                    (once - twice).abs() < 1e-4,
                    "ppp={ppp} size={size}: {once} -> {twice} と揺れた"
                );
            }
        }
    }

    #[test]
    fn snap_font_size_fails_soft_on_bad_input() {
        // 起動不能にしないため、異常値は panic せずそのまま返す。
        for bad_ppp in [0.0_f32, -1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(snap_font_size(14.0, bad_ppp).to_bits(), 14.0_f32.to_bits());
        }
        for bad_size in [0.0_f32, -3.0, f32::NAN] {
            assert_eq!(
                snap_font_size(bad_size, 1.25).to_bits(),
                bad_size.to_bits(),
                "異常サイズ {bad_size} を書き換えてはいけない"
            );
        }
        // 極小サイズでも最低 1 物理ピクセルは残す。
        assert!(snap_font_size(0.1, 1.0) * 1.0 >= 1.0);
    }

    #[test]
    fn snap_len_keeps_zero_and_sign() {
        for ppp in TEST_SCALES {
            assert_eq!(snap_len(0.0, ppp), 0.0, "ppp={ppp}: 0 の余白は 0 のまま");
            assert!(snap_len(-6.0, ppp) < 0.0, "ppp={ppp}: 符号が反転した");
            let s = snap_len(6.0, ppp);
            let px = s * ppp;
            assert!(
                (px - px.round()).abs() < 1e-3,
                "ppp={ppp}: {px}px が整数でない"
            );
        }
        assert_eq!(snap_len(6.0, f32::NAN).to_bits(), 6.0_f32.to_bits());
    }

    #[test]
    fn base_text_styles_are_pixel_exact_at_1x_and_2x() {
        // 表の値は整数のみ。等倍と 2 倍では丸めが一切起きてはいけない。
        for (ts, size, _) in base_text_styles() {
            assert_eq!(size, size.round(), "{ts:?}: 基準サイズ {size} が整数でない");
            for ppp in [1.0_f32, 2.0] {
                assert_eq!(
                    snap_font_size(size, ppp),
                    size,
                    "{ts:?}: ppp={ppp} で丸めが発生した"
                );
            }
        }
    }

    #[test]
    fn text_styles_preserve_the_size_hierarchy_at_every_scale() {
        // 丸めで Small > Body のような逆転が起きないことの退行検出。
        use egui::TextStyle;
        for ppp in TEST_SCALES {
            let t: std::collections::BTreeMap<TextStyle, f32> = text_styles(ppp)
                .into_iter()
                .map(|(ts, f)| (ts, f.size))
                .collect();
            let g = |ts: &TextStyle| *t.get(ts).unwrap_or_else(|| panic!("{ts:?} 欠落"));
            assert!(g(&TextStyle::Small) < g(&TextStyle::Body), "ppp={ppp}");
            assert!(g(&TextStyle::Monospace) < g(&TextStyle::Body), "ppp={ppp}");
            assert_eq!(g(&TextStyle::Body), g(&TextStyle::Button), "ppp={ppp}");
            assert!(g(&TextStyle::Body) < g(&TextStyle::Heading), "ppp={ppp}");
        }
    }

    #[test]
    fn text_styles_use_monospace_family_only_for_monospace() {
        use egui::{FontFamily, TextStyle};
        for ppp in TEST_SCALES {
            for (ts, font) in text_styles(ppp) {
                let want = if ts == TextStyle::Monospace {
                    FontFamily::Monospace
                } else {
                    FontFamily::Proportional
                };
                assert_eq!(font.family, want, "{ts:?} @ ppp={ppp}");
            }
        }
    }

    #[test]
    fn apply_writes_snapped_sizes_and_spacing_into_the_style() {
        let ctx = egui::Context::default();
        apply(&ctx, &by_name("zaivern-dark"));
        let ppp = ctx.pixels_per_point();
        let style = ctx.style();
        for (ts, size, _) in base_text_styles() {
            let got = style
                .text_styles
                .get(&ts)
                .unwrap_or_else(|| panic!("{ts:?} が Style に無い"));
            assert_eq!(got.size, snap_font_size(size, ppp), "{ts:?}");
        }
        assert_eq!(
            style.spacing.item_spacing,
            snap_vec2(BASE_ITEM_SPACING, ppp)
        );
        assert_eq!(
            style.spacing.button_padding,
            snap_vec2(BASE_BUTTON_PADDING, ppp)
        );
        assert_eq!(
            style.spacing.interact_size,
            snap_vec2(BASE_INTERACT_SIZE, ppp)
        );
    }

    #[test]
    fn apply_snaps_identically_for_dark_and_light_style_slots() {
        // テキストサイズはテーマ非依存。両スロットで食い違うと
        // OS のライト/ダーク切替で行高が飛ぶ。
        let ctx = egui::Context::default();
        apply(&ctx, &by_name("zaivern-light"));
        let dark = ctx.style_of(egui::Theme::Dark);
        let light = ctx.style_of(egui::Theme::Light);
        for (ts, _, _) in base_text_styles() {
            assert_eq!(
                dark.text_styles.get(&ts),
                light.text_styles.get(&ts),
                "{ts:?}"
            );
        }
        assert_eq!(dark.spacing.item_spacing, light.spacing.item_spacing);
    }

    #[test]
    fn resync_pixel_snapping_is_a_no_op_when_the_scale_is_unchanged() {
        // 毎フレーム走るフックなので、変化が無ければ何も書かないこと。
        // (書いてしまうとアイドル時のコストがゼロでなくなる)
        let ctx = egui::Context::default();
        apply(&ctx, &by_name("zaivern-dark"));
        assert!(!resync_pixel_snapping(&ctx));
        assert!(!resync_pixel_snapping(&ctx));
    }

    #[test]
    fn resync_pixel_snapping_follows_a_scale_change() {
        let ctx = egui::Context::default();
        apply(&ctx, &by_name("zaivern-dark"));
        // 125% のディスプレイへ移した状況を作る。
        ctx.set_pixels_per_point(1.25);
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let ppp = ctx.pixels_per_point();
        assert!(
            (ppp - 1.25).abs() < 1e-6,
            "テスト前提が崩れている: ppp={ppp}"
        );

        let style = ctx.style();
        for (ts, size, _) in base_text_styles() {
            let got = style
                .text_styles
                .get(&ts)
                .unwrap_or_else(|| panic!("{ts:?} が Style に無い"));
            assert_eq!(got.size, snap_font_size(size, 1.25), "{ts:?}");
            let px = got.size * 1.25;
            assert!(
                (px - px.round()).abs() < 1e-3,
                "{ts:?}: {px}px が整数でない"
            );
        }
        // 追従済みなので、もう一度呼んでも書き込みは起きない。
        assert!(!resync_pixel_snapping(&ctx));
    }

    #[test]
    fn repeated_apply_does_not_stack_begin_pass_hooks() {
        // on_begin_pass は呼ぶたびに積まれる。テーマ切替のたびに
        // フックが増えると、フレーム先頭のコストが際限なく伸びる。
        let ctx = egui::Context::default();
        for t in all() {
            apply(&ctx, &t);
        }
        apply(&ctx, &by_name("zaivern-dark"));
        // フック数は直接見られないので、代わりに「1 フレーム走らせても
        // スナップ状態が安定していること」を確認する。
        let before = ctx.style().text_styles.clone();
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        assert_eq!(ctx.style().text_styles, before);
        assert!(!resync_pixel_snapping(&ctx));
    }

    #[test]
    fn every_theme_ansi_normal_colors_are_distinct() {
        // ANSI 0..=7 (通常色) は互いに異なるはず。8..=15 (明色) は
        // 通常色の再利用を含む設計なので重複を許す。
        for t in all() {
            let mut normal: Vec<Color32> = t.ansi[..8].to_vec();
            normal.sort_unstable_by_key(|c| (c.r(), c.g(), c.b(), c.a()));
            normal.dedup();
            assert_eq!(normal.len(), 8, "{}: duplicate ansi normal color", t.name);
        }
    }
}
