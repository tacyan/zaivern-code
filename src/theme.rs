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
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.menu_margin = egui::Margin::same(8.0);

    use egui::{FontFamily, FontId, TextStyle};
    style.text_styles.insert(TextStyle::Body, FontId::new(13.5, FontFamily::Proportional));
    style.text_styles.insert(TextStyle::Button, FontId::new(13.5, FontFamily::Proportional));
    style.text_styles.insert(TextStyle::Small, FontId::new(11.0, FontFamily::Proportional));
    style.text_styles.insert(TextStyle::Heading, FontId::new(18.0, FontFamily::Proportional));
    style.text_styles.insert(TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace));

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
