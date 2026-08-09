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

/// 虹色括弧で塗り分ける深さの本数 (これを超えたら先頭へ折り返す)。
///
/// VS Code の既定と同じ 3 色。**11 種のテーマすべてで互いに見分けが付く**
/// 上限がここだった — 4 本目に緑を足すと Everforest 系 (`zaivern-forest`)
/// で黄と緑が距離 57 まで近づき、深さの違いが読めなくなる。
/// 色数を増やすより、どのテーマでも確実に読めるほうを採る
/// (`theme::tests::虹色括弧の色が全テーマで読めて見分けが付く` が番人)。
pub const BRACKET_DEPTHS: usize = 3;

/// sRGB の相対輝度 (WCAG 2.x)。
pub fn relative_luminance(col: Color32) -> f32 {
    let f = |v: u8| {
        let s = v as f32 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(col.r()) + 0.7152 * f(col.g()) + 0.0722 * f(col.b())
}

/// 2 色のコントラスト比 (WCAG 2.x)。1.0 (同色) 〜 21.0 (黒と白)。
pub fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let (x, y) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if x > y { (x, y) } else { (y, x) };
    (hi + 0.05) / (lo + 0.05)
}

/// 2 色の距離 (チェビシェフ距離)。「見分けが付くか」の粗い目安。
///
/// 知覚的な色差 (CIEDE2000 など) ではないが、テーマの配色を選ぶ用途には
/// 十分で、依存も計算量も増やさない。
fn rgb_distance(a: Color32, b: Color32) -> i32 {
    let d = |x: u8, y: u8| (x as i32 - y as i32).abs();
    d(a.r(), b.r()).max(d(a.g(), b.g())).max(d(a.b(), b.b()))
}

/// `a` を `b` へ `t` (0..=1) だけ寄せる。
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let f = |x: u8, y: u8| {
        (x as f32 + (y as f32 - x as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

/// `bg` の上で最低 `min` のコントラストになるまで `col` を明暗方向へ寄せる。
///
/// 寄せ先は白か黒 (地が暗ければ白、明るければ黒) — 色相を保ったまま
/// 明度だけ動かすので、読めるようにしたせいで色同士が潰れることがない。
fn readable_on(col: Color32, bg: Color32, min: f32) -> Color32 {
    let target = if relative_luminance(bg) < 0.5 {
        Color32::WHITE
    } else {
        Color32::BLACK
    };
    let mut out = col;
    for _ in 0..12 {
        if contrast_ratio(out, bg) >= min {
            break;
        }
        out = mix(out, target, 0.12);
    }
    out
}

impl Theme {
    /// 括弧の入れ子を深さごとに塗り分ける色 (VS Code の
    /// `editor.bracketPairColorization`)。
    ///
    /// **テーマの ANSI 表から採る** — 直書きしないので、テーマを変えれば
    /// 括弧の色も一緒に変わる。並びは「黄 → 紫 → 青」
    /// (VS Code の既定 Gold / Orchid / LightSkyBlue と同じ順)。
    /// 地に沈む色は明暗方向へ寄せてコントラスト 3:1 を確保する。
    pub fn bracket_colors(&self) -> [Color32; BRACKET_DEPTHS] {
        // ansi: 1=赤 2=緑 3=黄 4=青 5=紫 6=シアン
        // 赤はエラー色と、シアンは青と、緑は黄と紛れるテーマがあるので使わない。
        const IDX: [usize; BRACKET_DEPTHS] = [3, 5, 4];
        let mut out = [Color32::WHITE; BRACKET_DEPTHS];
        for (o, i) in out.iter_mut().zip(IDX) {
            *o = readable_on(self.ansi[i], self.bg, 3.0);
        }
        out
    }

    /// 縦のルーラー (`editor.rulers`) の線の色。
    /// 本文の邪魔にならないよう境界線と同じ濃さにする。
    /// **リンクの色。** 端末やエディタで「押せる」ことを示す唯一の合図。
    ///
    /// アクセント色を流用しない — アクセントは選択の塗り・カーソル・
    /// フォーカスリング・ドロップ枠にも使っており、同じ色でリンクを描くと
    /// 「押せるもの」と「いま選ばれているもの」が見分けられなくなる。
    ///
    /// 色相は**青から動かさない**。「リンクは青」はブラウザも OS も共有して
    /// いる合図で、テーマごとに変えると押せることが伝わらない。ただし
    /// 生の青ではなく**そのテーマが持つ青** (ANSI 12 = 明るい青) を使うので、
    /// 配色から浮かない。地の上で読める明るさまで寄せてある。
    pub fn link_color(&self) -> Color32 {
        // 青系の候補から、**アクセント色から最も遠いもの**を選ぶ。
        // Nordic のようにアクセント自体が青のテーマでは、素直に ANSI 12 を
        // 使うとリンクとカーソル/選択枠が同じ色になって見分けが付かない。
        // 候補はどれも「青〜シアン」なので、どれを選んでも
        //「リンクは青」の合図は保たれる。
        let cands = [self.ansi[12], self.ansi[4], self.ansi[14], self.ansi[6]];
        let best = cands
            .into_iter()
            .max_by_key(|c| rgb_distance(*c, self.accent))
            .unwrap_or(self.ansi[12]);
        readable_on(best, self.term_bg, 4.5)
    }

    pub fn ruler_color(&self) -> Color32 {
        self.border
    }
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
        // 補助テキストは白地に対して 4.5:1 (WCAG AA) を満たす濃さにする
        text_dim: c(0x6b, 0x71, 0x80),
        border: c(0xd8, 0xd8, 0xd2),
        term_bg: c(0xff, 0xff, 0xfe),
        term_fg: c(0x2c, 0x31, 0x3d),
        // 明るい緑 (#16a34a) / 黄 (#b48306) は白地パネルに対して 3:1 を割り、
        // バッジや診断の色として沈んでいた。基準を満たす濃さへ落とす。
        ok: c(0x15, 0x80, 0x3d),
        warn: c(0x9a, 0x6b, 0x00),
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

// ─── 追加テーマ ──────────────────────────────────────────────────
//
// 新しい配色を足すときの制約 (どれも回帰テストが守っている):
//   * `syntect_theme` は syntect の `ThemeSet::load_defaults()` に**実在する名前**。
//     外すとハイライトが黙って無地に落ちる (highlight.rs が None でプレーン化する)。
//   * ダークの `panel` は十分に暗く。file_tree の git バッジ (VS Code 準拠の
//     固定色) とのコントラスト比 3.0 を割ると回帰テストが落ちる。
//   * `accent_soft` は「地に近い淡色」。diff の hunk 背景がこれなので、
//     本文色 (`text`) と十分離れていないと差分ビューが読めなくなる。
//   * `ansi[0..8]` は互いに異なる色 (明色側 8..16 は再利用可)。

/// 北欧の青灰 (Nord 系)。寒色でコントラストは穏やか。
fn zaivern_nordic() -> Theme {
    Theme {
        name: "zaivern-nordic".into(),
        label: "Zaivern Nordic".into(),
        dark: true,
        bg: c(0x10, 0x14, 0x1b),
        panel: c(0x17, 0x1d, 0x27),
        panel_alt: c(0x13, 0x19, 0x22),
        accent: c(0x88, 0xc0, 0xd0),
        accent_soft: c(0x24, 0x31, 0x3f),
        text: c(0xe5, 0xe9, 0xf0),
        text_dim: c(0x8b, 0x98, 0xad),
        border: c(0x23, 0x2c, 0x3a),
        term_bg: c(0x0d, 0x11, 0x17),
        term_fg: c(0xd8, 0xde, 0xe9),
        ok: c(0xa3, 0xbe, 0x8c),
        warn: c(0xeb, 0xcb, 0x8b),
        err: c(0xbf, 0x61, 0x6a),
        ansi: [
            c(0x2e, 0x34, 0x40),
            c(0xbf, 0x61, 0x6a),
            c(0xa3, 0xbe, 0x8c),
            c(0xeb, 0xcb, 0x8b),
            c(0x81, 0xa1, 0xc1),
            c(0xb4, 0x8e, 0xad),
            c(0x88, 0xc0, 0xd0),
            c(0xd8, 0xde, 0xe9),
            c(0x4c, 0x56, 0x6a),
            c(0xcf, 0x6f, 0x78),
            c(0xb3, 0xcd, 0x9a),
            c(0xf2, 0xd7, 0x9c),
            c(0x8f, 0xb0, 0xd4),
            c(0xc4, 0x9d, 0xbd),
            c(0x8f, 0xbc, 0xbb),
            c(0xec, 0xef, 0xf4),
        ],
        syntect_theme: "base16-ocean.dark".into(),
    }
}

/// 炭火の暖色 (Gruvbox 系)。目に刺さらないレトロな色調。
fn zaivern_ember() -> Theme {
    Theme {
        name: "zaivern-ember".into(),
        label: "Zaivern Ember".into(),
        dark: true,
        bg: c(0x1a, 0x17, 0x14),
        panel: c(0x22, 0x1d, 0x19),
        panel_alt: c(0x1d, 0x19, 0x16),
        accent: c(0xfe, 0x80, 0x19),
        accent_soft: c(0x3b, 0x2a, 0x1c),
        text: c(0xec, 0xe0, 0xcd),
        text_dim: c(0xa8, 0x99, 0x84),
        border: c(0x33, 0x2b, 0x24),
        term_bg: c(0x17, 0x14, 0x11),
        term_fg: c(0xeb, 0xdb, 0xb2),
        ok: c(0xb8, 0xbb, 0x26),
        warn: c(0xfa, 0xbd, 0x2f),
        err: c(0xfb, 0x49, 0x34),
        ansi: [
            c(0x28, 0x28, 0x28),
            c(0xcc, 0x24, 0x1d),
            c(0x98, 0x97, 0x1a),
            c(0xd7, 0x99, 0x21),
            c(0x45, 0x85, 0x88),
            c(0xb1, 0x62, 0x86),
            c(0x68, 0x9d, 0x6a),
            c(0xa8, 0x99, 0x84),
            c(0x92, 0x83, 0x74),
            c(0xfb, 0x49, 0x34),
            c(0xb8, 0xbb, 0x26),
            c(0xfa, 0xbd, 0x2f),
            c(0x83, 0xa5, 0x98),
            c(0xd3, 0x86, 0x9b),
            c(0x8e, 0xc0, 0x7c),
            c(0xeb, 0xdb, 0xb2),
        ],
        syntect_theme: "base16-eighties.dark".into(),
    }
}

/// 深緑 (Everforest 系)。長時間の作業向けに彩度を落とした緑基調。
fn zaivern_forest() -> Theme {
    Theme {
        name: "zaivern-forest".into(),
        label: "Zaivern Forest".into(),
        dark: true,
        bg: c(0x0f, 0x16, 0x11),
        panel: c(0x16, 0x1e, 0x17),
        panel_alt: c(0x12, 0x1a, 0x14),
        accent: c(0xa7, 0xc0, 0x80),
        accent_soft: c(0x24, 0x33, 0x1f),
        text: c(0xe0, 0xe6, 0xd4),
        text_dim: c(0x93, 0xa1, 0x7f),
        border: c(0x23, 0x30, 0x1f),
        term_bg: c(0x0c, 0x12, 0x0e),
        term_fg: c(0xd3, 0xc6, 0xaa),
        ok: c(0x8f, 0xbf, 0x7f),
        warn: c(0xdb, 0xbc, 0x7f),
        err: c(0xe6, 0x7e, 0x80),
        ansi: [
            c(0x1e, 0x23, 0x26),
            c(0xe6, 0x7e, 0x80),
            c(0xa7, 0xc0, 0x80),
            c(0xdb, 0xbc, 0x7f),
            c(0x7f, 0xbb, 0xb3),
            c(0xd6, 0x99, 0xb6),
            c(0x83, 0xc0, 0x92),
            c(0xd3, 0xc6, 0xaa),
            c(0x4b, 0x56, 0x5c),
            c(0xef, 0x93, 0x95),
            c(0xb8, 0xd0, 0x93),
            c(0xe8, 0xcc, 0x94),
            c(0x94, 0xcb, 0xc4),
            c(0xe3, 0xac, 0xc6),
            c(0x97, 0xd0, 0xa5),
            c(0xf0, 0xed, 0xdf),
        ],
        syntect_theme: "base16-ocean.dark".into(),
    }
}

/// 深海の青緑 (Solarized Dark 系)。彩度を抑えた低刺激の配色。
fn zaivern_ocean() -> Theme {
    Theme {
        name: "zaivern-ocean".into(),
        label: "Zaivern Ocean".into(),
        dark: true,
        bg: c(0x00, 0x20, 0x28),
        panel: c(0x00, 0x2a, 0x35),
        panel_alt: c(0x00, 0x24, 0x2d),
        accent: c(0x2a, 0xa1, 0x98),
        accent_soft: c(0x07, 0x36, 0x42),
        text: c(0xd6, 0xe3, 0xe0),
        text_dim: c(0x8b, 0xa1, 0xa0),
        border: c(0x0b, 0x3c, 0x48),
        term_bg: c(0x00, 0x1b, 0x21),
        term_fg: c(0x9a, 0xa9, 0xa8),
        ok: c(0x85, 0x99, 0x00),
        warn: c(0xb5, 0x89, 0x00),
        err: c(0xdc, 0x32, 0x2f),
        ansi: [
            c(0x07, 0x36, 0x42),
            c(0xdc, 0x32, 0x2f),
            c(0x85, 0x99, 0x00),
            c(0xb5, 0x89, 0x00),
            c(0x26, 0x8b, 0xd2),
            c(0xd3, 0x36, 0x82),
            c(0x2a, 0xa1, 0x98),
            c(0xee, 0xe8, 0xd5),
            c(0x58, 0x6e, 0x75),
            c(0xe0, 0x5a, 0x57),
            c(0x9f, 0xb3, 0x00),
            c(0xcf, 0x9f, 0x00),
            c(0x4a, 0xa3, 0xe0),
            c(0xe0, 0x55, 0x9b),
            c(0x3d, 0xc0, 0xb5),
            c(0xfd, 0xf6, 0xe3),
        ],
        syntect_theme: "Solarized (dark)".into(),
    }
}

/// 純黒。OLED の消灯を活かした高コントラスト (省電力・暗所向け)。
fn zaivern_carbon() -> Theme {
    Theme {
        name: "zaivern-carbon".into(),
        label: "Zaivern Carbon".into(),
        dark: true,
        bg: c(0x00, 0x00, 0x00),
        panel: c(0x0a, 0x0a, 0x0a),
        panel_alt: c(0x05, 0x05, 0x05),
        accent: c(0x00, 0xd1, 0xff),
        accent_soft: c(0x10, 0x22, 0x2a),
        text: c(0xff, 0xff, 0xff),
        text_dim: c(0xa0, 0xa0, 0xa0),
        border: c(0x26, 0x26, 0x26),
        term_bg: c(0x00, 0x00, 0x00),
        term_fg: c(0xf2, 0xf2, 0xf2),
        ok: c(0x22, 0xe0, 0x6a),
        warn: c(0xff, 0xcc, 0x00),
        err: c(0xff, 0x5f, 0x5f),
        ansi: [
            c(0x1a, 0x1a, 0x1a),
            c(0xff, 0x5f, 0x5f),
            c(0x4e, 0xe8, 0x8a),
            c(0xff, 0xd1, 0x66),
            c(0x63, 0xb3, 0xff),
            c(0xd7, 0x8b, 0xff),
            c(0x5f, 0xe4, 0xe4),
            c(0xe6, 0xe6, 0xe6),
            c(0x4d, 0x4d, 0x4d),
            c(0xff, 0x87, 0x87),
            c(0x7f, 0xf0, 0xa8),
            c(0xff, 0xe0, 0x8a),
            c(0x8e, 0xc9, 0xff),
            c(0xe3, 0xb0, 0xff),
            c(0x93, 0xf2, 0xf2),
            c(0xff, 0xff, 0xff),
        ],
        syntect_theme: "base16-eighties.dark".into(),
    }
}

/// 葡萄酒の暗赤 (Rosé Pine 系)。既存のダークに無い**赤紫の地**で、
/// 暖色でも Ember (茶橙) と取り違えないよう地の色相を紫側へ振ってある。
fn zaivern_wine() -> Theme {
    Theme {
        name: "zaivern-wine".into(),
        label: "Zaivern Wine".into(),
        dark: true,
        bg: c(0x1b, 0x10, 0x16),
        panel: c(0x25, 0x1a, 0x21),
        panel_alt: c(0x1f, 0x14, 0x1b),
        accent: c(0xf0, 0x70, 0x9d),
        accent_soft: c(0x3d, 0x25, 0x31),
        text: c(0xf3, 0xe6, 0xec),
        text_dim: c(0xb3, 0x9b, 0xa6),
        border: c(0x3b, 0x26, 0x32),
        term_bg: c(0x18, 0x0d, 0x13),
        term_fg: c(0xec, 0xda, 0xe2),
        // 地が赤寄りなので、エラーだけは薔薇色のアクセントと混ざらない朱を使う。
        ok: c(0x6f, 0xcf, 0x8f),
        warn: c(0xe0, 0xa9, 0x4e),
        err: c(0xe7, 0x5f, 0x79),
        ansi: [
            c(0x2b, 0x1a, 0x23),
            c(0xe7, 0x5f, 0x79),
            c(0x8f, 0xc4, 0x7f),
            c(0xe0, 0xa9, 0x4e),
            c(0x8c, 0x9d, 0xf2),
            c(0xd9, 0x8a, 0xe0),
            c(0x6f, 0xc9, 0xc2),
            c(0xd9, 0xc4, 0xcd),
            c(0x5e, 0x42, 0x50),
            c(0xf4, 0x78, 0x8e),
            c(0xa8, 0xd6, 0x97),
            c(0xef, 0xc0, 0x6f),
            c(0xa5, 0xb2, 0xf7),
            c(0xe7, 0xa7, 0xed),
            c(0x8f, 0xdc, 0xd5),
            c(0xf8, 0xee, 0xf3),
        ],
        syntect_theme: "base16-mocha.dark".into(),
    }
}

/// 琥珀の CRT (アンバー端末)。**本文と端末の文字そのものを琥珀に染める**ので、
/// 同じ暖色でも Ember (文字は生成り) とは一目で違う。
/// ANSI は色相を残しつつ全体を暖色側へ寄せ、当時のモニタの雰囲気にする。
fn zaivern_amber() -> Theme {
    Theme {
        name: "zaivern-amber".into(),
        label: "Zaivern Amber".into(),
        dark: true,
        bg: c(0x16, 0x11, 0x0a),
        panel: c(0x1f, 0x18, 0x0f),
        panel_alt: c(0x1a, 0x14, 0x09),
        // P3 蛍光体のアンバー。端末の文字色と同じ色を UI のアクセントにも使う。
        accent: c(0xff, 0xb0, 0x00),
        accent_soft: c(0x3a, 0x2a, 0x0c),
        text: c(0xf2, 0xd9, 0xa4),
        text_dim: c(0xb0, 0x94, 0x68),
        border: c(0x36, 0x2a, 0x17),
        term_bg: c(0x13, 0x0e, 0x08),
        term_fg: c(0xff, 0xb0, 0x00),
        ok: c(0x9f, 0xbf, 0x3f),
        warn: c(0xff, 0xc9, 0x3c),
        err: c(0xe8, 0x60, 0x3c),
        ansi: [
            c(0x24, 0x1b, 0x0e),
            c(0xe8, 0x60, 0x3c),
            c(0x9f, 0xbf, 0x3f),
            c(0xff, 0xb0, 0x00),
            c(0x9a, 0xa8, 0xc8),
            c(0xd0, 0x8f, 0xa8),
            c(0x7f, 0xc4, 0xb0),
            c(0xe0, 0xc7, 0x9a),
            c(0x5c, 0x4a, 0x2e),
            c(0xf4, 0x7a, 0x55),
            c(0xb8, 0xd4, 0x5f),
            c(0xff, 0xcb, 0x52),
            c(0xb3, 0xc0, 0xdc),
            c(0xe0, 0xa8, 0xbf),
            c(0x9f, 0xd8, 0xc8),
            c(0xff, 0xe9, 0xc4),
        ],
        syntect_theme: "base16-eighties.dark".into(),
    }
}

/// 無彩色 + 真鍮の 1 アクセント。UI の彩度をほぼ捨てて、
/// **色が付いているものは意味を持つ**という状態にする
/// (アクセント = 選択、ok/warn/err = 診断。ここだけ色相が残る)。
/// 端末の ANSI は `ls --color` が死なないよう色相は残し、彩度だけ落とす。
fn zaivern_mono() -> Theme {
    Theme {
        name: "zaivern-mono".into(),
        label: "Zaivern Mono".into(),
        dark: true,
        bg: c(0x17, 0x19, 0x1a),
        panel: c(0x1e, 0x21, 0x23),
        panel_alt: c(0x19, 0x1c, 0x1d),
        accent: c(0xc9, 0xa2, 0x4a),
        accent_soft: c(0x33, 0x2c, 0x1c),
        text: c(0xe6, 0xe8, 0xe9),
        text_dim: c(0x9a, 0xa0, 0xa2),
        border: c(0x2d, 0x31, 0x33),
        term_bg: c(0x13, 0x15, 0x16),
        term_fg: c(0xdc, 0xdf, 0xe0),
        ok: c(0x7f, 0xb0, 0x8a),
        warn: c(0xe2, 0xb2, 0x3c),
        err: c(0xd0, 0x6a, 0x6a),
        ansi: [
            c(0x26, 0x29, 0x2b),
            c(0xc9, 0x80, 0x80),
            c(0x93, 0xb5, 0x8f),
            c(0xcf, 0xae, 0x5e),
            c(0x7f, 0xa0, 0xc9),
            c(0xb2, 0x94, 0xbd),
            c(0x7f, 0xb5, 0xb0),
            c(0xc4, 0xc8, 0xca),
            c(0x4c, 0x50, 0x52),
            c(0xd9, 0x9a, 0x9a),
            c(0xa9, 0xc7, 0xa5),
            c(0xe0, 0xc4, 0x7f),
            c(0x9b, 0xb8, 0xdc),
            c(0xc8, 0xaf, 0xd1),
            c(0x9b, 0xcb, 0xc6),
            c(0xe6, 0xea, 0xec),
        ],
        syntect_theme: "base16-ocean.dark".into(),
    }
}

/// ネオン (Monokai 系)。地はオリーブがかった暗灰で、
/// 前景だけを高彩度のライム / ピンク / 藤にする。
/// 既存のダークはどれも寒色か茶系の地なので、この地の色相が識別点になる。
fn zaivern_neon() -> Theme {
    Theme {
        name: "zaivern-neon".into(),
        label: "Zaivern Neon".into(),
        dark: true,
        bg: c(0x27, 0x28, 0x22),
        panel: c(0x21, 0x22, 0x1c),
        panel_alt: c(0x2c, 0x2d, 0x25),
        accent: c(0xa6, 0xe2, 0x2e),
        accent_soft: c(0x3a, 0x3d, 0x2a),
        text: c(0xf5, 0xf4, 0xea),
        text_dim: c(0xa8, 0xa8, 0x94),
        border: c(0x3b, 0x3d, 0x31),
        term_bg: c(0x1f, 0x20, 0x1a),
        term_fg: c(0xf0, 0xef, 0xe4),
        ok: c(0x7b, 0xd8, 0x8f),
        warn: c(0xe6, 0xdb, 0x74),
        err: c(0xf9, 0x26, 0x72),
        ansi: [
            c(0x2f, 0x30, 0x29),
            c(0xf9, 0x26, 0x72),
            c(0xa6, 0xe2, 0x2e),
            c(0xe6, 0xdb, 0x74),
            c(0x66, 0xd9, 0xef),
            c(0xae, 0x81, 0xff),
            c(0xa1, 0xef, 0xe4),
            c(0xf8, 0xf8, 0xf2),
            c(0x75, 0x71, 0x5e),
            c(0xff, 0x5c, 0x8a),
            c(0xb6, 0xec, 0x5a),
            c(0xf0, 0xe6, 0x8c),
            c(0x8c, 0xe5, 0xf5),
            c(0xc3, 0xa0, 0xff),
            c(0xba, 0xf5, 0xec),
            c(0xff, 0xff, 0xff),
        ],
        syntect_theme: "base16-eighties.dark".into(),
    }
}

/// 石板の青灰 (One Dark 系)。**同梱ダークの中でいちばん地が明るい**ので、
/// 真っ暗な画面が眩しさより疲れに効く人向けの選択肢になる。
fn zaivern_slate() -> Theme {
    Theme {
        name: "zaivern-slate".into(),
        label: "Zaivern Slate".into(),
        dark: true,
        bg: c(0x28, 0x2c, 0x34),
        panel: c(0x21, 0x25, 0x2b),
        panel_alt: c(0x2c, 0x30, 0x38),
        accent: c(0x61, 0xaf, 0xef),
        accent_soft: c(0x31, 0x3a, 0x48),
        // One Dark の本来の前景 (#abb2bf) は地に対して 7:1 に届かないので、
        // 色相はそのままに明度だけ上げてある。
        text: c(0xdf, 0xe5, 0xee),
        text_dim: c(0x98, 0xa0, 0xad),
        border: c(0x3a, 0x41, 0x50),
        term_bg: c(0x21, 0x25, 0x2b),
        term_fg: c(0xd4, 0xda, 0xe4),
        ok: c(0x98, 0xc3, 0x79),
        warn: c(0xe5, 0xc0, 0x7b),
        err: c(0xe0, 0x6c, 0x75),
        ansi: [
            c(0x3b, 0x40, 0x48),
            c(0xe0, 0x6c, 0x75),
            c(0x98, 0xc3, 0x79),
            c(0xe5, 0xc0, 0x7b),
            c(0x61, 0xaf, 0xef),
            c(0xc6, 0x78, 0xdd),
            c(0x56, 0xb6, 0xc2),
            c(0xab, 0xb2, 0xbf),
            c(0x5c, 0x63, 0x70),
            c(0xef, 0x8f, 0x97),
            c(0xb3, 0xd9, 0x9a),
            c(0xf0, 0xd1, 0x9b),
            c(0x8c, 0xc6, 0xf5),
            c(0xd7, 0x9a, 0xe8),
            c(0x7f, 0xcd, 0xd6),
            c(0xee, 0xf2, 0xf7),
        ],
        syntect_theme: "base16-ocean.dark".into(),
    }
}

/// 生成りの紙 (Solarized Light 系)。白飛びしない暖色のライト。
fn zaivern_paper() -> Theme {
    Theme {
        name: "zaivern-paper".into(),
        label: "Zaivern Paper".into(),
        dark: false,
        bg: c(0xfd, 0xf6, 0xe3),
        panel: c(0xf2, 0xea, 0xd3),
        panel_alt: c(0xec, 0xe3, 0xc8),
        accent: c(0xa3, 0x5d, 0x16),
        accent_soft: c(0xf0, 0xe2, 0xc4),
        text: c(0x3b, 0x32, 0x28),
        text_dim: c(0x6f, 0x63, 0x50),
        border: c(0xdd, 0xd3, 0xb4),
        term_bg: c(0xff, 0xfb, 0xf0),
        term_fg: c(0x45, 0x3b, 0x2e),
        ok: c(0x4d, 0x7c, 0x0f),
        warn: c(0xa1, 0x62, 0x07),
        err: c(0xb9, 0x1c, 0x1c),
        ansi: [
            c(0x46, 0x3c, 0x2f),
            c(0xc0, 0x39, 0x2b),
            c(0x4d, 0x7c, 0x0f),
            c(0xa1, 0x62, 0x07),
            c(0x1d, 0x6f, 0xa5),
            c(0x8e, 0x44, 0xad),
            c(0x0f, 0x76, 0x6e),
            c(0x6b, 0x61, 0x52),
            c(0x8a, 0x7f, 0x6d),
            c(0xc0, 0x39, 0x2b),
            c(0x4d, 0x7c, 0x0f),
            c(0xa1, 0x62, 0x07),
            c(0x1d, 0x6f, 0xa5),
            c(0x8e, 0x44, 0xad),
            c(0x0f, 0x76, 0x6e),
            c(0x2b, 0x24, 0x1b),
        ],
        syntect_theme: "Solarized (light)".into(),
    }
}

/// 白地に黒。屋外や外部ディスプレイ向けの高コントラストなライト。
fn zaivern_daylight() -> Theme {
    Theme {
        name: "zaivern-daylight".into(),
        label: "Zaivern Daylight".into(),
        dark: false,
        bg: c(0xff, 0xff, 0xff),
        panel: c(0xf4, 0xf4, 0xf5),
        panel_alt: c(0xea, 0xea, 0xec),
        accent: c(0x0b, 0x57, 0xd0),
        accent_soft: c(0xdb, 0xe6, 0xfb),
        text: c(0x10, 0x10, 0x14),
        text_dim: c(0x55, 0x55, 0x5e),
        border: c(0xc8, 0xc8, 0xcc),
        term_bg: c(0xff, 0xff, 0xff),
        term_fg: c(0x14, 0x14, 0x1a),
        ok: c(0x0a, 0x7a, 0x34),
        warn: c(0x8a, 0x5a, 0x00),
        err: c(0xc1, 0x12, 0x12),
        ansi: [
            c(0x1b, 0x1b, 0x20),
            c(0xc1, 0x12, 0x12),
            c(0x0a, 0x7a, 0x34),
            c(0x8a, 0x5a, 0x00),
            c(0x0b, 0x57, 0xd0),
            c(0x7a, 0x1f, 0xa2),
            c(0x00, 0x70, 0x6b),
            c(0x5c, 0x5c, 0x66),
            c(0x7a, 0x7a, 0x85),
            c(0xc1, 0x12, 0x12),
            c(0x0a, 0x7a, 0x34),
            c(0x8a, 0x5a, 0x00),
            c(0x0b, 0x57, 0xd0),
            c(0x7a, 0x1f, 0xa2),
            c(0x00, 0x70, 0x6b),
            c(0x10, 0x10, 0x14),
        ],
        syntect_theme: "InspiredGitHub".into(),
    }
}

/// 霜の青みがかったライト。白すぎず、寒色で締まった配色。
fn zaivern_frost() -> Theme {
    Theme {
        name: "zaivern-frost".into(),
        label: "Zaivern Frost".into(),
        dark: false,
        bg: c(0xf4, 0xf7, 0xfb),
        panel: c(0xe8, 0xee, 0xf7),
        panel_alt: c(0xdf, 0xe7, 0xf2),
        accent: c(0x2f, 0x6f, 0xd0),
        accent_soft: c(0xd6, 0xe2, 0xf5),
        text: c(0x1f, 0x2a, 0x3a),
        text_dim: c(0x5a, 0x6a, 0x80),
        border: c(0xc6, 0xd2, 0xe2),
        term_bg: c(0xfb, 0xfd, 0xff),
        term_fg: c(0x24, 0x30, 0x4a),
        ok: c(0x17, 0x80, 0x3d),
        warn: c(0x9a, 0x67, 0x00),
        err: c(0xc6, 0x28, 0x28),
        ansi: [
            c(0x2b, 0x36, 0x48),
            c(0xc6, 0x28, 0x28),
            c(0x17, 0x80, 0x3d),
            c(0x9a, 0x67, 0x00),
            c(0x2f, 0x6f, 0xd0),
            c(0x7b, 0x3f, 0xb5),
            c(0x0e, 0x74, 0x90),
            c(0x5b, 0x6b, 0x80),
            c(0x84, 0x94, 0xa8),
            c(0xc6, 0x28, 0x28),
            c(0x17, 0x80, 0x3d),
            c(0x9a, 0x67, 0x00),
            c(0x2f, 0x6f, 0xd0),
            c(0x7b, 0x3f, 0xb5),
            c(0x0e, 0x74, 0x90),
            c(0x1f, 0x2a, 0x3a),
        ],
        syntect_theme: "base16-ocean.light".into(),
    }
}

/// セージの淡緑 (Everforest Light 系)。同梱ライトに無かった**緑の地**で、
/// 白 (Daylight) や青 (Frost) より地の反射がやわらかい。
fn zaivern_sage() -> Theme {
    Theme {
        name: "zaivern-sage".into(),
        label: "Zaivern Sage".into(),
        dark: false,
        bg: c(0xee, 0xf4, 0xec),
        panel: c(0xe3, 0xeb, 0xe0),
        panel_alt: c(0xda, 0xe4, 0xd7),
        accent: c(0x3f, 0x7d, 0x5c),
        accent_soft: c(0xd9, 0xe8, 0xdc),
        text: c(0x23, 0x30, 0x2a),
        text_dim: c(0x5a, 0x6b, 0x60),
        border: c(0xcb, 0xd8, 0xc8),
        term_bg: c(0xf7, 0xfa, 0xf5),
        term_fg: c(0x26, 0x33, 0x2c),
        ok: c(0x2f, 0x7d, 0x32),
        warn: c(0x8a, 0x61, 0x00),
        err: c(0xb4, 0x26, 0x2a),
        ansi: [
            c(0x33, 0x40, 0x3a),
            c(0xb4, 0x26, 0x2a),
            c(0x2f, 0x7d, 0x32),
            c(0x8a, 0x61, 0x00),
            c(0x1f, 0x63, 0x90),
            c(0x7a, 0x3f, 0x9e),
            c(0x0f, 0x70, 0x68),
            c(0x64, 0x75, 0x6b),
            c(0x8b, 0x9b, 0x90),
            c(0xb4, 0x26, 0x2a),
            c(0x2f, 0x7d, 0x32),
            c(0x8a, 0x61, 0x00),
            c(0x1f, 0x63, 0x90),
            c(0x7a, 0x3f, 0x9e),
            c(0x0f, 0x70, 0x68),
            c(0x23, 0x30, 0x2a),
        ],
        syntect_theme: "base16-ocean.light".into(),
    }
}

/// 夜明けの桜色 (Rosé Pine Dawn 系)。Paper (黄みの生成り) に対して
/// 地を**赤紫側**へ振ってあるので、並べても取り違えない。
fn zaivern_dawn() -> Theme {
    Theme {
        name: "zaivern-dawn".into(),
        label: "Zaivern Dawn".into(),
        dark: false,
        bg: c(0xfa, 0xf4, 0xed),
        panel: c(0xf2, 0xe9, 0xe1),
        panel_alt: c(0xec, 0xe0, 0xd8),
        accent: c(0xb4, 0x63, 0x7a),
        accent_soft: c(0xf0, 0xdf, 0xe3),
        text: c(0x3f, 0x35, 0x40),
        text_dim: c(0x6b, 0x64, 0x78),
        border: c(0xe0, 0xd3, 0xcb),
        term_bg: c(0xfe, 0xfa, 0xf5),
        term_fg: c(0x43, 0x38, 0x4a),
        // 薔薇色のアクセントと紛れないよう、エラーは彩度の高い赤へ寄せる。
        ok: c(0x3e, 0x7d, 0x5a),
        warn: c(0x96, 0x69, 0x0b),
        err: c(0xb0, 0x2a, 0x2a),
        ansi: [
            c(0x4a, 0x3f, 0x4d),
            c(0xb0, 0x2a, 0x2a),
            c(0x3e, 0x7d, 0x5a),
            c(0x96, 0x69, 0x0b),
            c(0x28, 0x69, 0x83),
            c(0x81, 0x58, 0xa3),
            c(0x1f, 0x7a, 0x75),
            c(0x6b, 0x64, 0x78),
            c(0x8f, 0x87, 0x97),
            c(0xb0, 0x2a, 0x2a),
            c(0x3e, 0x7d, 0x5a),
            c(0x96, 0x69, 0x0b),
            c(0x28, 0x69, 0x83),
            c(0x81, 0x58, 0xa3),
            c(0x1f, 0x7a, 0x75),
            c(0x3f, 0x35, 0x40),
        ],
        syntect_theme: "InspiredGitHub".into(),
    }
}

/// 眩しさを抑えた高コントラスト。Daylight (純白 + 彩度の高い青) が
/// **光量で疲れる**人向けに、地を灰へ 1 段落として文字を限界まで濃くした
/// (本文と地のコントラスト比は同梱ライトで最も高い)。屋外・視覚過敏向け。
fn zaivern_quiet() -> Theme {
    Theme {
        name: "zaivern-quiet".into(),
        label: "Zaivern Quiet".into(),
        dark: false,
        bg: c(0xec, 0xeb, 0xe6),
        panel: c(0xe2, 0xe1, 0xdb),
        panel_alt: c(0xd8, 0xd7, 0xd0),
        accent: c(0x0d, 0x5c, 0x56),
        accent_soft: c(0xd3, 0xdd, 0xda),
        text: c(0x14, 0x16, 0x1a),
        text_dim: c(0x4a, 0x4d, 0x52),
        border: c(0xbc, 0xbb, 0xb4),
        term_bg: c(0xf2, 0xf1, 0xec),
        term_fg: c(0x17, 0x19, 0x1d),
        ok: c(0x1c, 0x6b, 0x2e),
        warn: c(0x7a, 0x52, 0x00),
        err: c(0xa8, 0x1c, 0x1c),
        ansi: [
            c(0x1f, 0x21, 0x24),
            c(0xa8, 0x1c, 0x1c),
            c(0x1c, 0x6b, 0x2e),
            c(0x7a, 0x52, 0x00),
            c(0x17, 0x45, 0x8f),
            c(0x6a, 0x2a, 0x94),
            c(0x0d, 0x5c, 0x56),
            c(0x4f, 0x52, 0x57),
            c(0x74, 0x77, 0x7c),
            c(0xa8, 0x1c, 0x1c),
            c(0x1c, 0x6b, 0x2e),
            c(0x7a, 0x52, 0x00),
            c(0x17, 0x45, 0x8f),
            c(0x6a, 0x2a, 0x94),
            c(0x0d, 0x5c, 0x56),
            c(0x14, 0x16, 0x1a),
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
// **このモジュールは `pixels_per_point` / `zoom_factor` を変更しない。**
// ここが担うのは「与えられた ppp に対して丸めを合わせる」ことだけで、
// 倍率そのものは持たない。UI 全体の拡大縮小 (ユーザーの ⌘+ / ⌘-) は
// `app::apply_ui_zoom` が `zoom_factor` を動かして行い、その結果として
// 変わった ppp へは下の `resync_pixel_snapping` フックが自動で追随する。
// 倍率の持ち主は `Config::ui_zoom` 1 つ — ここで別に持つと二重管理になる。

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

/// `ppp` と**文字サイズ倍率**に合わせてスナップ済みのテキストスタイル表を返す。
///
/// `scale` は [`Config::text_scale`](crate::config::Config::text_scale) —
/// 「余白やボタンの大きさは変えずに、文字だけ大きく / 小さく」のための倍率。
/// 画面全体のズーム (`ui_zoom`) は `ppp` の側に乗って届くので、ここでは
/// **二重に掛けない**。異常値は等倍へフェイルソフトする
/// (ここで panic すると設定ファイルの 1 文字で起動不能になる)。
pub fn text_styles(ppp: f32, scale: f32) -> Vec<(egui::TextStyle, egui::FontId)> {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    base_text_styles()
        .into_iter()
        .map(|(ts, size, fam)| {
            (
                ts,
                egui::FontId::new(snap_font_size(size * scale, ppp), fam),
            )
        })
        .collect()
}

// ── 文字サイズ倍率の置き場所 ────────────────────────────────────────
//
// 倍率の**持ち主は `Config::text_scale` 1 つ**。ここに置くのは
// 「毎フレームのスナップフックが読むための控え」で、`egui::Context` の
// 一時データに入れる。App の構造体に持たせないのは、スナップの規則と
// 材料が離れると片方だけ直されて壊れるため (ppp の控えと同じ理由)。

fn text_scale_id() -> egui::Id {
    egui::Id::new("zaivern::theme::text_scale")
}

/// いま効いている文字サイズ倍率 (未設定なら等倍)。
pub fn text_scale(ctx: &egui::Context) -> f32 {
    ctx.data(|d| d.get_temp::<f32>(text_scale_id()))
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(1.0)
}

/// 文字サイズ倍率を差し替える。変化があったら再描画を要求して true。
///
/// 実際のスタイル書き換えは次フレーム先頭の [`resync_pixel_snapping`] が
/// 行う — 書き換え地点を 1 つに保つことで、倍率と ppp の両方が動いたときに
/// 誤差が積み上がるのを防ぐ (毎回**基準値から**計算し直す)。
pub fn set_text_scale(ctx: &egui::Context, scale: f32) -> bool {
    let scale = crate::zoom::clamp(scale);
    if (text_scale(ctx) - scale).abs() < 1e-4 {
        return false;
    }
    ctx.data_mut(|d| d.insert_temp(text_scale_id(), scale));
    ctx.request_repaint();
    true
}

/// テキストサイズと余白を `ppp` の物理ピクセルグリッドへ合わせる。
/// 基準値からの再計算なので何度呼んでも同じ結果になる (冪等・誤差蓄積なし)。
fn apply_pixel_snapping(style: &mut egui::Style, ppp: f32, scale: f32) {
    for (ts, font) in text_styles(ppp, scale) {
        style.text_styles.insert(ts, font);
    }
    style.spacing.item_spacing = snap_vec2(BASE_ITEM_SPACING, ppp);
    style.spacing.button_padding = snap_vec2(BASE_BUTTON_PADDING, ppp);
    style.spacing.menu_margin = egui::Margin::same(snap_len(BASE_MENU_MARGIN, ppp));
    // ボタン等の最小寸法。端数スケールで 22.5px のような値になると
    // ウィジェットごとに 1px ズレて縦のリズムが崩れる。
    style.spacing.interact_size = snap_vec2(BASE_INTERACT_SIZE, ppp);
}

/// `ctx` に紐づけて覚えておく「最後にスナップした (pixels_per_point, 文字サイズ倍率)」。
///
/// 倍率まで含めるのが要点 — ppp だけを見ていると、⌘⇧+ で倍率だけ変えたときに
/// 「変化なし」と判断して**文字サイズが変わらない**。
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
    let scale = text_scale(ctx);
    let id = snapped_ppp_id();
    let last = ctx.data(|d| d.get_temp::<(f32, f32)>(id));
    if last.is_some_and(|l| l == (ppp, scale)) {
        return false;
    }
    ctx.data_mut(|d| d.insert_temp(id, (ppp, scale)));
    // テキストサイズはテーマに依存しないので、両テーマ分まとめて直す。
    ctx.all_styles_mut(|s| apply_pixel_snapping(s, ppp, scale));
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

/// 同梱テーマの一覧。**ダークを先、ライトを後**にまとめて返す
/// (メニューはこの順をそのまま 2 段の見出しに割る)。
pub fn all() -> Vec<Theme> {
    vec![
        // ダーク
        zaivern_dark(),
        zaivern_midnight(),
        zaivern_nordic(),
        zaivern_ember(),
        zaivern_forest(),
        zaivern_ocean(),
        zaivern_carbon(),
        zaivern_wine(),
        zaivern_amber(),
        zaivern_mono(),
        zaivern_neon(),
        zaivern_slate(),
        // ライト
        zaivern_light(),
        zaivern_paper(),
        zaivern_daylight(),
        zaivern_frost(),
        zaivern_sage(),
        zaivern_dawn(),
        zaivern_quiet(),
    ]
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
    apply_pixel_snapping(&mut style, ppp, text_scale(ctx));

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

    // いま焼いたサイズがどの (ppp, 文字サイズ倍率) のものかを記録し、
    // 追従フックを登録する。
    // (記録しておかないと次フレームで無駄に丸め直しが走る。
    //  **`resync_pixel_snapping` と同じ型で書くこと** — 片方だけ変えると
    //  `get_temp` が型不一致で None を返し、毎フレーム丸め直しが走る)
    //  **倍率は `data_mut` に入る前に読むこと** — クロージャの中で
    //  `text_scale(ctx)` を呼ぶと、書き込みロックを握ったまま同じ
    //  `Memory` の読み取りロックを取りに行って**デッドロックする**
    //  (parking_lot の RwLock は再入不可。テストが CPU 0% のまま
    //   永久に固まり、cargo のロック待ちが repo 全体へ波及した)。
    let scale = text_scale(ctx);
    ctx.data_mut(|d| d.insert_temp(snapped_ppp_id(), (ppp, scale)));
    install_pixel_snap_plugin(ctx);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- all ----

    #[test]
    fn all_returns_the_builtin_themes_in_order() {
        let names: Vec<String> = all().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            [
                "zaivern-dark",
                "zaivern-midnight",
                "zaivern-nordic",
                "zaivern-ember",
                "zaivern-forest",
                "zaivern-ocean",
                "zaivern-carbon",
                "zaivern-wine",
                "zaivern-amber",
                "zaivern-mono",
                "zaivern-neon",
                "zaivern-slate",
                "zaivern-light",
                "zaivern-paper",
                "zaivern-daylight",
                "zaivern-frost",
                "zaivern-sage",
                "zaivern-dawn",
                "zaivern-quiet",
            ]
        );
    }

    /// メニューは `all()` の並びをそのまま「ダーク → ライト」の 2 段に割る。
    /// 途中でライトが混ざると見出しが交互に出て一覧が読めなくなる。
    #[test]
    fn all_lists_every_dark_theme_before_the_light_ones() {
        let themes = all();
        let first_light = themes
            .iter()
            .position(|t| !t.dark)
            .expect("ライトテーマが 1 つも無い");
        assert!(
            themes[first_light..].iter().all(|t| !t.dark),
            "ライトの後ろにダークが混ざっている"
        );
        assert!(first_light >= 2, "ダークテーマが少なすぎる");
    }

    /// 明暗どちらも複数用意する (片側 1 つだけだと「選べない」に等しい)。
    #[test]
    fn both_dark_and_light_have_multiple_choices() {
        let themes = all();
        assert!(
            themes.iter().filter(|t| t.dark).count() >= 4,
            "ダークが少ない"
        );
        assert!(
            themes.iter().filter(|t| !t.dark).count() >= 3,
            "ライトが少ない"
        );
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

    /// 同じ明暗の中で、テーマ同士が**目で見分けが付く**こと。
    ///
    /// 名前と label が違っても、地とアクセントがほぼ同じなら一覧に並べる意味が無い
    /// (「増やしたが選べない」= 機能を増やす前に減らせ、の反例になる)。
    /// 閾値 72 は同梱で最も近い既存の組 (Daylight / Frost = 83) の下に置いてある。
    #[test]
    fn 同梱テーマは互いに見分けが付く() {
        let themes = all();
        for t in &themes {
            assert!(t.name.starts_with("zaivern-"), "{}: 接頭辞が無い", t.name);
        }
        for (i, a) in themes.iter().enumerate() {
            for b in &themes[i + 1..] {
                if a.dark != b.dark {
                    continue;
                }
                let d = dist(a.bg, b.bg) + dist(a.accent, b.accent);
                assert!(
                    d >= 72,
                    "{} と {} の地/アクセントが近すぎる (距離 {d})",
                    a.name,
                    b.name
                );
            }
        }
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
    fn zaivern_light_is_a_light_theme() {
        let t = zaivern_light();
        assert_eq!(t.name, "zaivern-light");
        assert_eq!(t.label, "Zaivern Light");
        assert!(!t.dark);
        assert_eq!(t.syntect_theme, "InspiredGitHub");
    }

    /// `dark` フラグと実際の背景の明るさが食い違っていないこと。
    /// (ここがズレると egui の Visuals とアイコンの明暗が逆になる)
    #[test]
    fn the_dark_flag_matches_the_actual_background_brightness() {
        for t in all() {
            let l = relative_luminance(t.bg);
            if t.dark {
                assert!(l < 0.2, "{}: dark なのに背景が明るい ({l:.3})", t.name);
            } else {
                assert!(l > 0.5, "{}: light なのに背景が暗い ({l:.3})", t.name);
            }
            // パネルは地と同系の明るさ (片方だけ反転していると画面が割れて見える)
            let p = relative_luminance(t.panel);
            assert_eq!(
                t.dark,
                p < 0.5,
                "{}: panel の明るさがテーマの明暗と逆 ({p:.3})",
                t.name
            );
        }
    }

    /// 本文と補助テキストが背景に対して読める (WCAG 相対輝度によるコントラスト比)。
    /// 新しいテーマを足したときに「雰囲気は良いが読めない」配色を弾く。
    #[test]
    fn every_theme_meets_the_text_contrast_floor() {
        for t in all() {
            for (name, fg, bg, floor) in [
                ("text/bg", t.text, t.bg, 7.0),
                ("text/panel", t.text, t.panel, 7.0),
                ("text_dim/bg", t.text_dim, t.bg, 4.5),
                ("term_fg/term_bg", t.term_fg, t.term_bg, 7.0),
                // diff の hunk 背景 (accent_soft) の上にも本文を置く
                ("text/accent_soft", t.text, t.accent_soft, 4.5),
            ] {
                let r = contrast_ratio(fg, bg);
                assert!(
                    r >= floor,
                    "{}: {name} のコントラストが {r:.2} (最低 {floor})",
                    t.name
                );
            }
        }
    }

    /// 状態色 (成功/警告/エラー) がパネルの上で判別できること。
    /// バッジや診断の色なので、地に沈むと状態が伝わらない。
    #[test]
    fn every_theme_status_colors_stand_out_on_the_panel() {
        for t in all() {
            for (name, col) in [("ok", t.ok), ("warn", t.warn), ("err", t.err)] {
                let r = contrast_ratio(col, t.panel);
                assert!(r >= 3.0, "{}: {name} が panel に埋もれる ({r:.2})", t.name);
            }
            assert_ne!(t.ok, t.err, "{}: 成功とエラーが同色", t.name);
        }
    }

    /// syntect のテーマ名は実在するものだけ。
    /// 名前を間違えると highlight.rs が黙って無地にフォールバックし、
    /// 「そのテーマだけ色が付かない」という気づきにくい壊れ方をする。
    #[test]
    fn every_syntect_theme_name_exists_in_the_default_set() {
        let ts = syntect::highlighting::ThemeSet::load_defaults();
        for t in all() {
            assert!(
                ts.themes.contains_key(&t.syntect_theme),
                "{}: syntect に '{}' は無い (候補: {:?})",
                t.name,
                t.syntect_theme,
                ts.themes.keys().collect::<Vec<_>>()
            );
        }
    }

    /// 虹色括弧の色が、既定のテーマすべてで「読めて・見分けが付く」こと。
    ///
    /// 色はテーマの ANSI 表から採るので、テーマを足したときにここが落ちる
    /// = そのテーマでは括弧の深さが判別できない、という検出器になる。
    #[test]
    fn 虹色括弧の色が全テーマで読めて見分けが付く() {
        for t in all() {
            let cols = t.bracket_colors();
            assert_eq!(cols.len(), BRACKET_DEPTHS);
            for (i, col) in cols.iter().enumerate() {
                let r = contrast_ratio(*col, t.bg);
                assert!(r >= 3.0, "{}: 深さ {i} の括弧が地に沈む ({r:.2})", t.name);
                // エラー色 (対応が取れない括弧) と紛れない
                let d = dist(*col, t.err);
                assert!(
                    d >= 48,
                    "{}: 深さ {i} がエラー色と紛れる (距離 {d})",
                    t.name
                );
            }
            for i in 0..cols.len() {
                for j in (i + 1)..cols.len() {
                    let d = dist(cols[i], cols[j]);
                    assert!(
                        d >= 60,
                        "{}: 深さ {i} と {j} が見分けられない (距離 {d})",
                        t.name
                    );
                }
            }
        }
    }

    /// ルーラーの色はテーマの境界線色 (直書きしていない)。
    /// **どのテーマでもリンクが読めて、本文とも見分けが付く。**
    /// ここが崩れると「青いからリンクだと分かる」が成立しない。
    #[test]
    fn リンクの色は全テーマで読めて本文と見分けが付く() {
        for t in all() {
            let link = t.link_color();
            let c = contrast_ratio(link, t.term_bg);
            assert!(
                c >= 4.5,
                "{}: リンクが地に沈む (コントラスト {c:.2})",
                t.name
            );
            assert!(
                dist(link, t.term_fg) >= 40,
                "{}: リンクが本文と見分けが付かない",
                t.name
            );
            // アクセントと同じ色にしない (選択・カーソルと混ざる)。
            assert!(
                dist(link, t.accent) >= 30,
                "{}: リンクがアクセントと近すぎる",
                t.name
            );
        }
    }

    #[test]
    fn ルーラーの色はテーマから来る() {
        for t in all() {
            assert_eq!(t.ruler_color(), t.border, "{}", t.name);
            // 地と同色だと 1px も見えない
            assert_ne!(t.ruler_color(), t.bg, "{}", t.name);
        }
    }

    /// チャンネルごとの差の合計 (見分けが付くかの粗い指標)。
    fn dist(a: Color32, b: Color32) -> i32 {
        (a.r() as i32 - b.r() as i32).abs()
            + (a.g() as i32 - b.g() as i32).abs()
            + (a.b() as i32 - b.b() as i32).abs()
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
            let t: std::collections::BTreeMap<TextStyle, f32> = text_styles(ppp, 1.0)
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

    /// **文字サイズ倍率は文字だけを動かす。**
    /// これが崩れると「画面は広いまま字だけ大きく」が成立しない。
    #[test]
    fn 文字サイズ倍率は全スタイルへ比例して効く() {
        use egui::TextStyle;
        // 期待値は **スナップ前の基準サイズ × 倍率**。スナップ済みの値へ
        // 倍率を掛けると二重丸めになり、実装が正しくても落ちる
        // (丸めは必ず基準値から 1 回だけ、がこのモジュールの規約)。
        let base: std::collections::BTreeMap<TextStyle, f32> = base_text_styles()
            .into_iter()
            .map(|(ts, size, _)| (ts, size))
            .collect();
        for ppp in TEST_SCALES {
            for scale in [0.5_f32, 0.8, 1.25, 2.0, 3.0] {
                for (ts, font) in text_styles(ppp, scale) {
                    let want = base[&ts] * scale;
                    // スナップのずれは必ず 1 物理ピクセル未満 (= 0.5/ppp)。
                    // 少し余裕を持たせて 1 物理ピクセルぶんで見る。
                    let slack = 1.0 / ppp;
                    assert!(
                        (font.size - want).abs() <= slack,
                        "{ts:?} @ ppp={ppp} scale={scale}: {} と {want} が離れすぎ",
                        font.size
                    );
                }
            }
        }
    }

    /// 異常値 (0 / 負 / NaN) は等倍へフェイルソフトする。
    /// ここで潰れると、設定ファイルの 1 文字で**文字が消えて操作不能**になる。
    #[test]
    fn 文字サイズ倍率の異常値は等倍へ落とす() {
        for bad in [0.0_f32, -1.0, f32::NAN, f32::INFINITY] {
            let got = text_styles(1.0, bad);
            let want = text_styles(1.0, 1.0);
            assert_eq!(
                got.iter().map(|(_, f)| f.size).collect::<Vec<_>>(),
                want.iter().map(|(_, f)| f.size).collect::<Vec<_>>(),
                "scale={bad}"
            );
        }
    }

    /// 倍率を掛けても大小関係が崩れない (丸めで Small > Body の逆転をしない)。
    #[test]
    fn 文字サイズ倍率を掛けても大小関係が保たれる() {
        use egui::TextStyle;
        for ppp in TEST_SCALES {
            for scale in [0.5_f32, 0.9, 1.25, 2.0, 3.0] {
                let t: std::collections::BTreeMap<TextStyle, f32> = text_styles(ppp, scale)
                    .into_iter()
                    .map(|(ts, f)| (ts, f.size))
                    .collect();
                let g = |ts: &TextStyle| t[ts];
                assert!(
                    g(&TextStyle::Small) < g(&TextStyle::Body),
                    "ppp={ppp} scale={scale}"
                );
                assert!(
                    g(&TextStyle::Body) < g(&TextStyle::Heading),
                    "ppp={ppp} scale={scale}"
                );
                assert_eq!(
                    g(&TextStyle::Body),
                    g(&TextStyle::Button),
                    "ppp={ppp} scale={scale}"
                );
            }
        }
    }

    #[test]
    fn text_styles_use_monospace_family_only_for_monospace() {
        use egui::{FontFamily, TextStyle};
        for ppp in TEST_SCALES {
            for (ts, font) in text_styles(ppp, 1.0) {
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
