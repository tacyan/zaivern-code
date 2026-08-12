//! GUI E2E — **本物の描画関数**をヘッドレスに回して画面の破綻を捕まえる。
//!
//! ## なぜ要るか
//!
//! このリポジトリのテストは 4,600 本以上あるが、ほぼ全てが「純関数を表で
//! 固定する」形である。つまり**レイアウト関数の戻り値**は守られているが、
//! **その戻り値を使って実際に描いた結果**は誰も見ていない。
//! そのため次の壊れ方は全部素通りする:
//!
//! - ダイアログが画面の外へ出て掴めない
//! - ボタンが可視領域の外に落ちて押せない
//! - 行が可用幅を超えて見切れる
//! - 設定ファイルが無い初回起動 / 古い設定を読んだ直後に落ちる
//!
//! ## 2 層のうちの「層 1」
//!
//! ここは `eframe` を起動しない。`egui::Context` を直に回し、
//! `ctx.run()` が返す形 (`Shape`) と `Memory` の窓矩形を読む。
//! そのため **GPU も窓もフォントも要らず、CI の Linux でそのまま回る**。
//! 実バイナリを起こす「層 2」は `tools/gui-smoke.sh`。
//!
//! ## フォントについて
//!
//! `install_fonts` は OS のフォント (ヒラギノ / 游ゴシック / Noto CJK) を
//! 積むが、ここでは**積まない**。egui 同梱フォントだけで回すことで、
//! 「フォントが入っていない CI コンテナでは幅が変わって落ちる」という
//! 環境依存を構造的に断つ。日本語は豆腐 (□) になるが、
//! `Galley::text()` は元の文字列を返すので**文字の照合には影響しない**し、
//! 豆腐にも幅があるので**溢れの検出**も効く。
//!
//! ## 罠: `ctx.input()` の中で `ctx.*` を呼ばない
//!
//! egui 0.29 の `Context::input` / `input_mut` / `data` / `memory` は
//! どれも `Context::write` (**排他** RwLock) を取る。つまり
//! `ctx.input(|i| (i.focused, ctx.pixels_per_point()))` は**自分自身を待って
//! 永久に止まる** (実測: `parking_lot::RawRwLock::lock_exclusive_slow` で停止。
//! CPU は 0%、テストは無言で 10 分以上返ってこない)。
//! 欲しい値は全部 `InputState` の側にある —
//! `i.pixels_per_point()` / `i.screen_rect` / `i.viewport()` を使うこと。
//!
//! ## 実 `~/.zaivern` に触れないこと
//!
//! 設定・監査ログを読み書きする経路は `crate::test_util::unique_temp_dir` の
//! 下だけを使う (`ApprovalQueue::in_dir` 等)。環境変数の差し替えは
//! **並列に走る他のテストへ漏れる**ので使わない。

#![cfg(test)]

use egui::epaint::Shape;
use egui::{Pos2, Rect, Vec2};

// ── 画面 (ヘッドレスの窓) ────────────────────────────────────────────────

/// 1 枚の窓。`ctx.run()` を好きな大きさで何フレームでも回せる。
pub struct Screen {
    pub ctx: egui::Context,
    pub size: Vec2,
    pub theme: crate::theme::Theme,
    time: f64,
    /// 画面矩形の**左上**。既定は原点だが、サブディスプレイに置かれた窓では
    /// 原点ではない (macOS は主ディスプレイの左上を 0 とし、左や上に置いた
    /// 画面は**負の座標**になる)。
    origin: Pos2,
    /// OS が申告する物理ピクセル密度。Retina = 2.0、Windows の 125% = 1.25。
    /// `None` なら egui の既定 (1.0)。
    ppp: Option<f32>,
    /// モニタの実寸 (点)。`fullscreen_guard` が「窓 > モニタ」を見るのに
    /// 使う値と同じ口。
    monitor: Option<Vec2>,
    /// この窓がキーボードフォーカスを持っているか。
    focused: bool,
    /// ネイティブ全画面か (`ViewportInfo::fullscreen`)。
    fullscreen: Option<bool>,
}

/// 検査に使う「実際に描かれたもの」。
#[derive(Clone)]
pub struct Painted {
    /// 描かれた文字。
    pub texts: Vec<PaintedText>,
    /// 浮いている層 (窓・メニュー・ツールチップ) の矩形。
    pub layers: Vec<(egui::LayerId, Rect)>,
    /// 窓の大きさ。
    pub screen: Rect,
}

/// 1 個の文字形。
#[derive(Clone)]
pub struct PaintedText {
    pub text: String,
    /// 画面上の矩形。
    pub rect: Rect,
    /// この形に効いているクリップ矩形 (これを超えた部分は**見えない**)。
    pub clip: Rect,
}

impl PaintedText {
    /// 横方向に**途中で切られている**か。
    ///
    /// 「クリップ矩形と重なっているのに、はみ出してもいる」= 利用者には
    /// 単語の途中で切れた文字列が見える。完全に外にあるものは
    /// スクロールで送られただけなので数えない。
    pub fn horizontally_clipped(&self) -> bool {
        let inter = self.rect.intersect(self.clip);
        if inter.width() <= 0.5 || inter.height() <= 0.5 {
            return false; // まったく見えていない = スクロールの外
        }
        self.rect.min.x < self.clip.min.x - 0.5 || self.rect.max.x > self.clip.max.x + 0.5
    }
}

impl Screen {
    /// 既定のテーマで窓を 1 枚作る。
    pub fn new(w: f32, h: f32) -> Self {
        Self::with_theme(w, h, &crate::theme::all()[0].name.clone())
    }

    pub fn with_theme(w: f32, h: f32, theme_name: &str) -> Self {
        let ctx = egui::Context::default();
        let theme = crate::theme::by_name(theme_name);
        crate::theme::apply(&ctx, &theme);
        Screen {
            ctx,
            size: egui::vec2(w, h),
            theme,
            time: 0.0,
            origin: Pos2::ZERO,
            ppp: None,
            monitor: None,
            focused: true,
            fullscreen: None,
        }
    }

    /// **物理ピクセル密度**を差し替える (Retina = 2.0 / Windows の 125% = 1.25)。
    ///
    /// `theme::apply` が積んだ `on_begin_pass` フックが次のフレームの先頭で
    /// `resync_pixel_snapping` を回すので、**文字サイズと余白もこの ppp へ
    /// 丸め直される**。つまり「倍率を変えたら丸めがズレた絵になる」経路まで
    /// まるごと再現できる。
    pub fn ppp(mut self, ppp: f32) -> Self {
        self.ppp = Some(ppp);
        self
    }

    /// 画面矩形の左上を動かす (= 窓をサブディスプレイに置く)。
    ///
    /// 原点が (0,0) でない画面でだけ壊れるコード — 例えば `avail.width()` から
    /// 幅を出しておきながら位置を `Pos2::ZERO` 起点で組むもの — は、
    /// ここを負や大きな正の値にした瞬間に落ちる。
    pub fn at(mut self, origin: Pos2) -> Self {
        self.origin = origin;
        self
    }

    /// モニタの実寸を申告する (`ViewportInfo::monitor_size`)。
    pub fn monitor(mut self, size: Vec2) -> Self {
        self.monitor = Some(size);
        self
    }

    /// キーボードフォーカスの有無 (`⌘Tab` で裏へ回った状態を作る)。
    pub fn focus(mut self, on: bool) -> Self {
        self.focused = on;
        self
    }

    /// ネイティブ全画面かどうかを申告する (`ViewportInfo::fullscreen`)。
    pub fn fullscreen(mut self, on: bool) -> Self {
        self.fullscreen = Some(on);
        self
    }

    pub fn rect(&self) -> Rect {
        Rect::from_min_size(self.origin, self.size)
    }

    fn raw(&mut self, events: Vec<egui::Event>) -> egui::RawInput {
        self.time += 1.0 / 60.0;
        let mut raw = egui::RawInput {
            screen_rect: Some(self.rect()),
            time: Some(self.time),
            events,
            focused: self.focused,
            ..Default::default()
        };
        // `RawInput::default()` は ROOT の `ViewportInfo` を 1 つ持っている。
        // 作り直さず**書き換える**ことで、egui 側が増やした項目を取りこぼさない。
        let id = raw.viewport_id;
        if let Some(v) = raw.viewports.get_mut(&id) {
            v.native_pixels_per_point = self.ppp;
            v.inner_rect = Some(self.rect());
            v.outer_rect = Some(self.rect());
            v.monitor_size = self.monitor;
            v.focused = Some(self.focused);
            v.fullscreen = self.fullscreen;
        }
        raw
    }

    /// 1 フレーム回す。`f` は `ctx` を受けて好きなものを返す。
    pub fn run<R>(
        &mut self,
        events: Vec<egui::Event>,
        mut f: impl FnMut(&egui::Context) -> R,
    ) -> (R, Painted) {
        let raw = self.raw(events);
        let mut ret: Option<R> = None;
        let out = self.ctx.run(raw, |ctx| {
            ret = Some(f(ctx));
        });
        let mut painted = Painted {
            texts: Vec::new(),
            layers: Vec::new(),
            screen: self.rect(),
        };
        for cs in &out.shapes {
            collect_text(&cs.shape, cs.clip_rect, &mut painted.texts);
        }
        self.ctx.memory(|m| {
            for layer in m.areas().visible_layer_ids() {
                if let Some(r) = m.area_rect(layer.id) {
                    painted.layers.push((layer, r));
                }
            }
        });
        painted
            .layers
            .sort_by_key(|(l, _)| (format!("{:?}", l.order), l.id.value()));
        (ret.expect("ctx.run はクロージャを必ず 1 回呼ぶ"), painted)
    }

    /// `CentralPanel` の中で `f` を呼びながら 1 フレーム回す。
    pub fn panel<R>(
        &mut self,
        events: Vec<egui::Event>,
        mut f: impl FnMut(&mut egui::Ui) -> R,
    ) -> (R, Painted) {
        self.run(events, move |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| f(ui)).inner
        })
    }

    /// 何も操作せず `n` フレーム回して、最後のフレームを返す。
    ///
    /// egui は当たり判定に**前フレームの widget 矩形**を使うので、
    /// 押す前に最低 1 フレーム描いておく必要がある。
    pub fn settle<R>(&mut self, n: usize, mut f: impl FnMut(&egui::Context) -> R) -> (R, Painted) {
        let mut last = None;
        for _ in 0..n.max(1) {
            last = Some(self.run(Vec::new(), &mut f));
        }
        last.expect("n >= 1")
    }

    /// `pos` を**実際にクリック**して、そのフレームの戻り値を返す。
    ///
    /// 押下と解放を別フレームに分ける (egui は解放フレームで `clicked()` を
    /// 立てる)。当たらなければ戻り値は「何も押されていない」ままになる —
    /// つまり**画面外や別の層の下に隠れたボタンは、ここで落ちる**。
    pub fn click<R>(&mut self, pos: Pos2, mut f: impl FnMut(&egui::Context) -> R) -> R {
        // 1) 直前の状態を作る (widget 矩形の登録)
        self.settle(2, &mut f);
        // 2) 押下
        let press = vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        self.run(press, &mut f);
        // 3) 解放 (このフレームで clicked() が立つ)
        let release = vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }];
        self.run(release, &mut f).0
    }

    /// `CentralPanel` の中身に対する [`Screen::click`]。
    pub fn click_panel<R>(&mut self, pos: Pos2, mut f: impl FnMut(&mut egui::Ui) -> R) -> R {
        self.click(pos, move |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| f(ui)).inner
        })
    }
}

impl Painted {
    /// 本文に `needle` を含む最初の文字形。
    pub fn find(&self, needle: &str) -> Option<&PaintedText> {
        self.texts.iter().find(|t| t.text.contains(needle))
    }

    /// 本文に `needle` を含む文字形の中心 (= クリック先)。
    pub fn center_of(&self, needle: &str) -> Option<Pos2> {
        self.find(needle).map(|t| t.rect.center())
    }

    /// 全部の文字を 1 本につなげたもの (失敗時のメッセージ用)。
    pub fn joined(&self) -> String {
        self.texts
            .iter()
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 横方向に切れている文字形。
    pub fn clipped_texts(&self) -> Vec<&PaintedText> {
        self.texts
            .iter()
            .filter(|t| t.horizontally_clipped())
            .collect()
    }

    /// 浮いている層のうち、**窓とメニュー**だけ (背景パネルと当たり判定用の
    /// 見えない層を除く)。
    pub fn windows(&self) -> Vec<(egui::LayerId, Rect)> {
        self.layers
            .iter()
            .filter(|(l, r)| {
                !matches!(l.order, egui::Order::Background) && r.width() > 1.0 && r.height() > 1.0
            })
            .copied()
            .collect()
    }
}

/// `Shape` を再帰的にたどって文字形を集める。
fn collect_text(shape: &Shape, clip: Rect, out: &mut Vec<PaintedText>) {
    match shape {
        // **`TextShape::pos` は左上ではなく「アンカー」である。**
        // 中央揃え (`Align::Center`) の galley は `rect` が
        // `-w/2 ..= +w/2` になるので、`pos + size` で矩形を作ると
        // 全部が右へ半分ずれる (実際に仕様パネルの中央寄せを
        // 「はみ出している」と誤検出した)。必ず `galley.rect` を平行移動する。
        Shape::Text(t) => out.push(PaintedText {
            text: t.galley.text().to_string(),
            rect: t.galley.rect.translate(t.pos.to_vec2()),
            clip,
        }),
        Shape::Vec(v) => {
            for s in v {
                collect_text(s, clip, out);
            }
        }
        _ => {}
    }
}

// ── 検査そのもの (純関数。壊れた入力で必ず落ちることを下のテストで固定する) ──

/// `rects` が全部 `bounds` の中に収まっているか。外れたものを返す。
///
/// `tol` は丸め誤差の許容 (px)。
pub fn outside(bounds: Rect, rects: &[(String, Rect)], tol: f32) -> Vec<String> {
    let grown = bounds.expand(tol);
    rects
        .iter()
        .filter(|(_, r)| !grown.contains_rect(*r))
        .map(|(n, r)| format!("{n} {r:?} が {bounds:?} からはみ出した"))
        .collect()
}

/// `rects` が互いに重なっていないか。重なった組を返す。
pub fn overlapping(rects: &[(String, Rect)], tol: f32) -> Vec<String> {
    let mut bad = Vec::new();
    for i in 0..rects.len() {
        for j in (i + 1)..rects.len() {
            let (an, a) = &rects[i];
            let (bn, b) = &rects[j];
            let inter = a.intersect(*b);
            if inter.width() > tol && inter.height() > tol {
                bad.push(format!("{an} {a:?} と {bn} {b:?} が重なった ({inter:?})"));
            }
        }
    }
    bad
}

/// 「押せる」ことの最低条件: 面積があり、窓の中にあること。
pub fn unclickable(bounds: Rect, targets: &[(String, Rect)]) -> Vec<String> {
    let mut bad = Vec::new();
    for (n, r) in targets {
        if r.width() <= 0.0 || r.height() <= 0.0 {
            bad.push(format!("{n}: 面積が 0 ({r:?})"));
        } else if !bounds.expand(0.5).contains(r.center()) {
            bad.push(format!("{n}: 中心が窓の外 ({r:?} / 窓 {bounds:?})"));
        }
    }
    bad
}

/// テストでよく使う極端な窓の大きさ。
///
/// 「縦に極端」「横に極端」「小さい」「普通」「大きい」を 1 本に集める。
pub const SIZES: &[(f32, f32)] = &[
    (900.0, 700.0),
    (1200.0, 300.0),
    (520.0, 900.0),
    (1920.0, 1080.0),
    (360.0, 640.0),
];

// ── 土台そのものの自己検査 ──────────────────────────────────────────────

#[cfg(test)]
mod harness_tests {
    use super::*;

    /// 文字が拾えて、位置も入っている。
    #[test]
    fn 描いた文字と位置が拾える() {
        let mut s = Screen::new(400.0, 200.0);
        let (_, p) = s.panel(Vec::new(), |ui| {
            ui.label("ZAIVERN-E2E");
        });
        let t = p.find("ZAIVERN-E2E").expect("文字が拾えない");
        assert!(
            t.rect.width() > 0.0 && t.rect.height() > 0.0,
            "{:?}",
            t.rect
        );
        assert!(s.rect().contains_rect(t.rect), "画面の外に描かれた");
    }

    /// **クリックが本当に届く**。届かないなら以降の「押せる」テストは全部嘘になる。
    #[test]
    fn 模擬クリックが本物のボタンに届く() {
        let mut s = Screen::new(400.0, 200.0);
        let (_, p) = s.settle(2, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let _ = ui.button("PUSH-ME");
            });
        });
        let pos = p.center_of("PUSH-ME").expect("ボタンの文字が無い");
        let hit = s.click(pos, |ctx| {
            egui::CentralPanel::default()
                .show(ctx, |ui| ui.button("PUSH-ME").clicked())
                .inner
        });
        assert!(hit, "画面内のボタンにクリックが届かない (土台が壊れている)");
    }

    /// 画面の外を押しても当たらない = 当たり判定が本物である証拠。
    #[test]
    fn 画面外を押しても当たらない() {
        let mut s = Screen::new(400.0, 200.0);
        let hit = s.click(egui::pos2(9999.0, 9999.0), |ctx| {
            egui::CentralPanel::default()
                .show(ctx, |ui| ui.button("PUSH-ME").clicked())
                .inner
        });
        assert!(!hit, "画面外のクリックが当たってしまった");
    }

    /// 溢れの検出が効く。**わざと溢れさせて捕まることを固定する。**
    #[test]
    fn 溢れの検出はわざと壊すと捕まる() {
        let inside = PaintedText {
            text: "ok".into(),
            rect: Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(50.0, 10.0)),
            clip: Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 100.0)),
        };
        assert!(!inside.horizontally_clipped());
        let cut = PaintedText {
            rect: Rect::from_min_max(egui::pos2(60.0, 0.0), egui::pos2(160.0, 10.0)),
            ..inside.clone()
        };
        assert!(cut.horizontally_clipped(), "切れているのに捕まらない");
        // 完全に外 (スクロールで送られただけ) は数えない
        let scrolled = PaintedText {
            rect: Rect::from_min_max(egui::pos2(300.0, 0.0), egui::pos2(400.0, 10.0)),
            ..inside.clone()
        };
        assert!(!scrolled.horizontally_clipped(), "スクロール外を誤検出した");
    }

    /// はみ出し / 重なり / 押せない の 3 検査は、壊れた入力で必ず訴える。
    #[test]
    fn 三つの検査はわざと壊すと訴える() {
        let win = Rect::from_min_size(Pos2::ZERO, egui::vec2(100.0, 100.0));
        let good = vec![
            (
                "a".to_string(),
                Rect::from_min_size(Pos2::ZERO, egui::vec2(40.0, 40.0)),
            ),
            (
                "b".to_string(),
                Rect::from_min_size(egui::pos2(50.0, 0.0), egui::vec2(40.0, 40.0)),
            ),
        ];
        assert!(outside(win, &good, 0.5).is_empty());
        assert!(overlapping(&good, 0.5).is_empty());
        assert!(unclickable(win, &good).is_empty());

        let out_of_window = vec![(
            "dlg".to_string(),
            Rect::from_min_size(egui::pos2(80.0, 80.0), egui::vec2(60.0, 60.0)),
        )];
        assert_eq!(
            outside(win, &out_of_window, 0.5).len(),
            1,
            "画面外を見逃した"
        );
        assert_eq!(
            unclickable(win, &out_of_window).len(),
            1,
            "窓の外の押し先を見逃した"
        );

        let stacked = vec![
            (
                "a".to_string(),
                Rect::from_min_size(Pos2::ZERO, egui::vec2(40.0, 40.0)),
            ),
            (
                "b".to_string(),
                Rect::from_min_size(egui::pos2(20.0, 20.0), egui::vec2(40.0, 40.0)),
            ),
        ];
        assert_eq!(overlapping(&stacked, 0.5).len(), 1, "重なりを見逃した");

        let zero = vec![("z".to_string(), Rect::from_min_size(Pos2::ZERO, Vec2::ZERO))];
        assert_eq!(unclickable(win, &zero).len(), 1, "面積 0 を見逃した");
    }
}

#[cfg(test)]
mod probe {
    use super::*;
    #[test]
    #[ignore]
    fn 窓枠の実測() {
        for &(w, h) in &[(1200.0_f32, 300.0_f32), (900.0, 700.0)] {
            for mode in ["素", "scroll", "scroll+constrain"] {
                let mut s = Screen::new(w, h);
                let (_, p) = s.settle(3, |ctx| {
                    let screen = ctx.screen_rect();
                    let mut win = egui::Window::new("T")
                        .collapsible(false)
                        .resizable(false)
                        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0));
                    if mode == "scroll+constrain" {
                        win = win.constrain_to(screen.shrink(8.0));
                    }
                    win.show(ctx, |ui| {
                        let rows = |ui: &mut egui::Ui| {
                            for i in 0..60 {
                                ui.label(format!("row {i}"));
                            }
                        };
                        if mode == "素" {
                            rows(ui);
                        } else {
                            egui::ScrollArea::vertical().show(ui, rows);
                        }
                    });
                });
                for (l, r) in p.windows() {
                    println!("{w}x{h} {mode} {:?} -> {r:?} h={}", l.order, r.height());
                }
            }
        }
    }
}

// ── 層 1-a: ダイアログが画面の外へ出ない ────────────────────────────────

#[cfg(test)]
mod dialog_tests {
    use super::*;

    /// 開いた窓が全部画面の中に居るか調べる。
    fn assert_windows_on_screen(s: &mut Screen, what: &str, mut f: impl FnMut(&egui::Context)) {
        let (w, h) = (s.size.x, s.size.y);
        let (_, p) = s.settle(3, |ctx| f(ctx));
        let wins = p.windows();
        assert!(
            !wins.is_empty(),
            "{what} {w}x{h}: 窓が 1 つも開かなかった (テストが何も見ていない)"
        );
        let named: Vec<(String, Rect)> = wins
            .iter()
            .map(|(l, r)| (format!("{what}/{:?}", l.order), *r))
            .collect();
        let bad = outside(p.screen, &named, 1.0);
        assert!(
            bad.is_empty(),
            "{what} {w}x{h}: ダイアログが画面外へ出た:\n{}",
            bad.join("\n")
        );
        // 掴めない窓は無いのと同じ。中心が画面内にあることまで見る。
        let bad = unclickable(p.screen, &named);
        assert!(
            bad.is_empty(),
            "{what} {w}x{h}: 掴めない窓がある:\n{}",
            bad.join("\n")
        );
    }

    #[test]
    fn 衝突レーダーはどの窓の大きさでも画面内に収まる() {
        for &(w, h) in SIZES {
            let mut s = Screen::new(w, h);
            let theme = s.theme.clone();
            let radar = crate::conflict::ConflictRadar::default();
            let mut open = true;
            let mut sel = None;
            assert_windows_on_screen(&mut s, "衝突レーダー", |ctx| {
                let _ = crate::conflict::radar_window(ctx, &theme, &mut open, &radar, &mut sel);
            });
        }
    }

    #[test]
    fn エージェント追加はどの窓の大きさでも画面内に収まる() {
        for &(w, h) in SIZES {
            let mut s = Screen::new(w, h);
            let theme = s.theme.clone();
            let mut picker = crate::agent_picker::AgentPicker::default();
            picker.open = true;
            let presets: Vec<crate::config::AgentPreset> = Vec::new();
            assert_windows_on_screen(&mut s, "エージェント追加", |ctx| {
                let _ = crate::agent_picker::ui(&mut picker, ctx, &theme, &presets);
            });
        }
    }

    #[test]
    fn 調停のフォームはどの窓の大きさでも画面内に収まる() {
        for &(w, h) in SIZES {
            let theme = crate::theme::all()[0].clone();
            let rows: Vec<crate::orchestration::SessionRow> = Vec::new();
            let owners = std::collections::BTreeMap::new();

            let mut s = Screen::new(w, h);
            let mut st = crate::orchestration::OrchState::default();
            st.form_open = true;
            assert_windows_on_screen(&mut s, "新しいタスク", |ctx| {
                let _ = crate::orchestration::task_form_ui(&mut st, ctx, &theme, &rows, &owners);
            });

            let mut s = Screen::new(w, h);
            let mut st = crate::orchestration::OrchState::default();
            st.msg_open = true;
            assert_windows_on_screen(&mut s, "エージェントへ送信", |ctx| {
                let _ = crate::orchestration::message_form_ui(&mut st, ctx, &theme, &rows);
            });
        }
    }

    #[test]
    fn ローカルヒストリはどの窓の大きさでも画面内に収まる() {
        for &(w, h) in SIZES {
            let root = crate::test_util::unique_temp_dir("zv-e2e", "lh");
            let cfg = crate::config::Config::default();
            let mut lh = crate::local_history::LocalHistory::new(root.clone(), &cfg);
            lh.open = true;
            let mut s = Screen::new(w, h);
            assert_windows_on_screen(&mut s, "ローカルヒストリ", |ctx| lh.ui(ctx));
            let _ = std::fs::remove_dir_all(&root);
        }
    }
}

// ── 層 1-b: 主要な操作のボタンが本当に押せる ────────────────────────────
//
// 「面積が 0 でない」だけでは足りない。**実際にポインタを送って**、
// アプリ側の戻り値が変わることまで見る。押せない原因 (画面外・別の層に
// 覆われている・当たり判定が別の場所) はどれもここで落ちる。

#[cfg(test)]
mod click_tests {
    use super::*;

    /// パレット: 行を押すとその項目が返る。
    #[test]
    fn パレットの行はどの幅でも押せる() {
        for &(w, h) in SIZES {
            let items: Vec<crate::palette::Item> = (0..6)
                .map(|i| crate::palette::Item {
                    icon: "📄".into(),
                    label: format!("zv-e2e-item-{i}"),
                    detail: format!("src/zv_e2e_{i}.rs"),
                    action: crate::palette::Action::OpenFile(std::path::PathBuf::from(format!(
                        "src/zv_e2e_{i}.rs"
                    ))),
                    score: 0,
                })
                .collect();
            let st = crate::palette::Palette::new();
            let res = st.results(items, &[]);
            let theme = crate::theme::all()[0].clone();

            let mut s = Screen::new(w, h);
            let (_, p) = s.settle(2, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    crate::palette::list_ui(ui, &theme, &res, 0, false);
                });
            });
            let pos = p
                .center_of("zv-e2e-item-2")
                .unwrap_or_else(|| panic!("{w}x{h}: パレットの行が描かれていない\n{}", p.joined()));
            let hit = s.click_panel(pos, |ui| {
                crate::palette::list_ui(ui, &theme, &res, 0, false)
            });
            let hit = hit.unwrap_or_else(|| {
                panic!("{w}x{h}: パレットの行 (zv-e2e-item-2) を押しても何も返らない")
            });
            assert_eq!(hit.label, "zv-e2e-item-2", "{w}x{h}: 別の行が反応した");
        }
    }

    /// 承認パネル: 「監査ログ」を押すと読み直し要求が立つ。
    ///
    /// 監査ログの置き場は**一時ディレクトリ**へ向ける
    /// (`ApprovalQueue::in_dir`)。実 `~/.zaivern` は 1 バイトも触らない。
    #[test]
    fn 承認パネルの監査ログボタンはどの幅でも押せる() {
        for &(w, h) in SIZES {
            let dir = crate::test_util::unique_temp_dir("zv-e2e", "approve");
            let mut q = crate::agents::approvals::ApprovalQueue::in_dir(dir.clone());
            for i in 0..3u64 {
                q.intake(
                    i + 1,
                    Some("claude"),
                    "Do you want to proceed?\n 1. Yes\n 2. No, and tell Claude what to do",
                    100 + i,
                );
            }
            let theme = crate::theme::all()[0].clone();
            let mut expanded = std::collections::HashSet::new();
            let mut show_audit = false;

            let mut s = Screen::new(w, h);
            let (_, p) = s.settle(2, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    crate::panels::approvals_ui(
                        ui,
                        &theme,
                        &q,
                        &mut expanded,
                        &mut show_audit,
                        None,
                        0,
                    );
                });
            });
            let pos = p
                .center_of("監査ログ")
                .unwrap_or_else(|| panic!("{w}x{h}: 監査ログのボタンが無い\n{}", p.joined()));
            let out = s.click_panel(pos, |ui| {
                crate::panels::approvals_ui(ui, &theme, &q, &mut expanded, &mut show_audit, None, 0)
            });
            assert!(
                out.reload_audit,
                "{w}x{h}: 監査ログのボタンを押しても反応しない"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// エージェント追加: 「＋ 追加」を押すとプリセットが返る。
    /// **ダイアログの中**のボタンなので、窓が画面外へ出た瞬間に落ちる。
    #[test]
    fn エージェント追加のボタンはどの窓の大きさでも押せる() {
        for &(w, h) in SIZES {
            let theme = crate::theme::all()[0].clone();
            let presets: Vec<crate::config::AgentPreset> = Vec::new();
            let mut picker = crate::agent_picker::AgentPicker::default();
            picker.open = true;

            let mut s = Screen::new(w, h);
            let (_, p) = s.settle(3, |ctx| {
                let _ = crate::agent_picker::ui(&mut picker, ctx, &theme, &presets);
            });
            let Some(pos) = p.center_of("＋ 追加") else {
                // 低い窓ではスクロールの外に落ちることがある。
                // その場合でも**窓の中に 1 つは押せるものがある**ことは
                // dialog_tests が見ているので、ここは飛ばす。
                continue;
            };
            let act = s.click(pos, |ctx| {
                crate::agent_picker::ui(&mut picker, ctx, &theme, &presets)
            });
            assert!(
                matches!(act, Some(crate::agent_picker::PickerAction::Add { .. })),
                "{w}x{h}: 「＋ 追加」を押してもプリセットが返らない"
            );
        }
    }
}

// ── 層 1-c: どの幅でも行が見切れない ────────────────────────────────────

#[cfg(test)]
mod overflow_tests {
    use super::*;

    /// 「切れている文字」が 1 つも無いことを見る。
    fn assert_no_clipping(s: &mut Screen, what: &str, mut f: impl FnMut(&mut egui::Ui)) {
        let (w, h) = (s.size.x, s.size.y);
        let (_, p) = s.panel(Vec::new(), |ui| f(ui));
        let bad = p.clipped_texts();
        assert!(
            bad.is_empty(),
            "{what} {w}x{h}: {} 個の行が可用幅からはみ出して切れている:\n{}",
            bad.len(),
            bad.iter()
                .take(8)
                .map(|t| format!("  {:?} rect={:?} clip={:?}", t.text, t.rect, t.clip))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn 承認パネルはどの幅でも行が切れない() {
        for &(w, h) in SIZES {
            let dir = crate::test_util::unique_temp_dir("zv-e2e", "approve-w");
            let mut q = crate::agents::approvals::ApprovalQueue::in_dir(dir.clone());
            q.intake(
                1,
                Some("claude"),
                // わざと長い 1 行を混ぜる (省略が効いていないと必ず切れる)
                "Bash(cargo test --bin zai --all-features -- --nocapture --test-threads=1 とても長いコマンド行)\n 1. Yes\n 2. No",
                7,
            );
            let theme = crate::theme::all()[0].clone();
            let mut expanded = std::collections::HashSet::new();
            let mut show_audit = false;
            let mut s = Screen::new(w, h);
            assert_no_clipping(&mut s, "承認パネル", |ui| {
                crate::panels::approvals_ui(
                    ui,
                    &theme,
                    &q,
                    &mut expanded,
                    &mut show_audit,
                    None,
                    0,
                );
            });
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn パレットはどの幅でも行が切れない() {
        for &(w, h) in SIZES {
            let items: Vec<crate::palette::Item> = (0..8)
                .map(|i| crate::palette::Item {
                    icon: "📄".into(),
                    label: format!(
                        "とても長いラベル {i} — src/very/deeply/nested/path/to/a/file_{i}.rs"
                    ),
                    detail: format!("src/very/deeply/nested/path/to/a/file_{i}.rs"),
                    action: crate::palette::Action::OpenFile(std::path::PathBuf::from("a.rs")),
                    score: 0,
                })
                .collect();
            let st = crate::palette::Palette::new();
            let res = st.results(items, &[]);
            let theme = crate::theme::all()[0].clone();
            let mut s = Screen::new(w, h);
            assert_no_clipping(&mut s, "パレット", |ui| {
                crate::palette::list_ui(ui, &theme, &res, 0, false);
            });
        }
    }

    /// **パレットは候補を全部描く。**
    ///
    /// 直したのはこれ: 行を `ui.with_layout(right_to_left(Center))` で
    /// 組むと、子 Ui が `max_rect` を縦いっぱいに取るので 1 行目が残りの
    /// 高さを全部食う。900×700 で **8 件のうち 1 件しか描かれていなかった**。
    /// (`crate::palette::list_ui` の `allocate_ui_with_layout` を戻すと落ちる。)
    #[test]
    fn パレットは候補を全部描く() {
        let n = 8usize;
        let items: Vec<crate::palette::Item> = (0..n)
            .map(|i| crate::palette::Item {
                icon: "📄".into(),
                label: format!("zv-e2e-row-{i}"),
                detail: String::new(),
                action: crate::palette::Action::OpenFile(std::path::PathBuf::from("a.rs")),
                score: 0,
            })
            .collect();
        let st = crate::palette::Palette::new();
        let res = st.results(items, &[]);
        assert_eq!(res.rows.len(), n, "前提: 行が {n} 本あること");
        let theme = crate::theme::all()[0].clone();
        // 高い窓ほど「1 行目が残りを食う」壊れ方が目立つ。
        for &(w, h) in &[(900.0_f32, 700.0_f32), (1920.0, 1080.0), (520.0, 900.0)] {
            let mut s = Screen::new(w, h);
            let (_, p) = s.panel(Vec::new(), |ui| {
                crate::palette::list_ui(ui, &theme, &res, 0, false);
            });
            for i in 0..n {
                let needle = format!("zv-e2e-row-{i}");
                assert!(
                    p.find(&needle).is_some(),
                    "{w}x{h}: {needle} が描かれていない ({} 件しか出ていない)\n{}",
                    p.texts.len(),
                    p.joined()
                );
            }
            // 一覧の高さは窓に収まる (縦を食い潰していない)。
            let bottom = p
                .texts
                .iter()
                .map(|t| t.rect.max.y)
                .fold(f32::MIN, f32::max);
            assert!(bottom <= h, "{w}x{h}: 一覧の下端 {bottom} が窓の外へ出た");
        }
    }

    #[test]
    fn 仕様とスキルとmcpのパネルはどの幅でも行が切れない() {
        for &(w, h) in SIZES {
            let theme = crate::theme::all()[0].clone();

            let mut s = Screen::new(w, h);
            let mut spec = crate::spec::SpecPanel::default();
            assert_no_clipping(&mut s, "仕様パネル", |ui| {
                crate::spec::ui(ui, &theme, &mut spec);
            });

            let mut s = Screen::new(w, h);
            let mut skills = crate::skills::SkillsPanel::default();
            assert_no_clipping(&mut s, "スキルパネル", |ui| {
                crate::skills::ui(ui, &theme, &mut skills);
            });

            let mut s = Screen::new(w, h);
            let mut mcp = crate::mcp::McpPanel::default();
            assert_no_clipping(&mut s, "MCP パネル", |ui| {
                crate::mcp::ui(ui, &theme, &mut mcp);
            });
        }
    }

    /// レイアウト関数が返す矩形は、**どの大きさでも領域内に収まり重ならない**。
    /// (`UI の原則` の最後の項をそのまま検査にしたもの。)
    #[test]
    fn 空状態カードは領域内に収まる() {
        for &(w, h) in SIZES {
            let avail = Rect::from_min_size(Pos2::ZERO, egui::vec2(w, h));
            for presets in 0..8usize {
                let c = crate::panels::empty_card(avail, presets);
                let bad = outside(avail, &[("空状態カード".into(), c.card)], 0.5);
                assert!(bad.is_empty(), "{w}x{h} presets={presets}: {bad:?}");
            }
            for rows in 0..6usize {
                for buttons in 0..5usize {
                    let c = crate::panels::media_card(avail, rows, buttons);
                    let bad = outside(avail, &[("メディアカード".into(), c.card)], 0.5);
                    assert!(bad.is_empty(), "{w}x{h} rows={rows} btn={buttons}: {bad:?}");
                }
            }
        }
    }

    /// ブックマークの一覧レイアウトは、3 つの矩形が領域内に収まり**重ならない**。
    #[test]
    fn ブックマークの分割は重ならない() {
        for &(w, h) in SIZES {
            let area = Rect::from_min_size(Pos2::ZERO, egui::vec2(w, h));
            for ratio in [0.0_f32, 0.1, 0.45, 0.9, 1.0] {
                let l = crate::marks::panel_layout(area, ratio);
                let rects = vec![
                    ("tree".to_string(), l.tree),
                    ("splitter".to_string(), l.splitter),
                    ("preview".to_string(), l.preview),
                ];
                let bad = outside(area, &rects, 0.5);
                assert!(bad.is_empty(), "{w}x{h} ratio={ratio}: {}", bad.join("\n"));
                let bad = overlapping(&rects, 0.5);
                assert!(bad.is_empty(), "{w}x{h} ratio={ratio}: {}", bad.join("\n"));
            }
        }
    }
}

// ── 層 1-d: 初回起動と設定移行 ──────────────────────────────────────────

#[cfg(test)]
mod boot_tests {
    use super::*;
    use std::path::PathBuf;

    /// 設定を読んで、テーマを当てて、実際のパネルを 1 枚描くところまで。
    /// **落ちないこと**と**テーマが解決すること**を見る。
    fn boot_and_draw(home: &std::path::Path, roots: &[PathBuf]) -> crate::config::Config {
        let cfg = crate::config::load_from_dir(home, roots, true);
        let theme = crate::theme::by_name(&cfg.theme);
        let mut s = Screen::new(1200.0, 800.0);
        crate::theme::apply(&s.ctx, &theme);
        let mut spec = crate::spec::SpecPanel::default();
        let mut skills = crate::skills::SkillsPanel::default();
        let (_, p) = s.panel(Vec::new(), |ui| {
            crate::spec::ui(ui, &theme, &mut spec);
            crate::skills::ui(ui, &theme, &mut skills);
        });
        assert!(!p.texts.is_empty(), "1 文字も描かれなかった");
        cfg
    }

    /// **初回起動**: `~/.zaivern` が空でも落ちず、既定のエージェントが載る。
    #[test]
    fn 設定ファイルが1つも無くても起動して描ける() {
        let home = crate::test_util::unique_temp_dir("zv-e2e", "first-run");
        let root = crate::test_util::unique_temp_dir("zv-e2e", "first-ws");
        assert!(
            !home.join("config.toml").exists(),
            "前提が崩れている: 空のはずの home に config.toml がある"
        );
        let cfg = boot_and_draw(&home, std::slice::from_ref(&root));
        assert!(!cfg.agents.is_empty(), "既定のエージェントが載らない");
        assert!(
            !crate::theme::by_name(&cfg.theme).name.is_empty(),
            "テーマが解決しない"
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **アップデート後の設定移行**: 古い形式でも落ちない。
    ///
    /// - もう存在しないキー (`pet_speed` など) が残っている
    /// - 新しいキーが 1 つも無い
    /// - `agents` が空
    /// - `state.toml` が古い (今より項目が少ない)
    ///
    /// どれも「黙って既定へ倒す」のが正解で、**落ちるのは不正解**。
    #[test]
    fn 古い形式の設定を読んでも落ちない() {
        let home = crate::test_util::unique_temp_dir("zv-e2e", "migrate");
        let root = crate::test_util::unique_temp_dir("zv-e2e", "migrate-ws");
        std::fs::write(
            home.join("config.toml"),
            "# v0.4 時代の設定\n\
             theme = \"zaivern-dark\"\n\
             editor_font_size = 14.0\n\
             pet_speed = 3\n\
             legacy_sidebar_width = 220\n\
             [[agents]]\n\
             name = \"Claude\"\n\
             command = \"claude\"\n\
             icon = \"🤖\"\n",
        )
        .expect("write old config");
        std::fs::write(
            home.join("state.toml"),
            "theme = \"zaivern-light\"\nshow_pet = true\n",
        )
        .expect("write old state");

        let cfg = boot_and_draw(&home, std::slice::from_ref(&root));
        // state.toml が config.toml を上書きするのが仕様。
        assert_eq!(cfg.theme, "zaivern-light", "state.toml の theme が効かない");
        assert_eq!(cfg.agents.len(), 1, "古い agents が引き継がれない");
        assert_eq!(cfg.agents[0].name, "Claude");
        // 壊れた扱いにして退避していないこと (知らないキーは無視するだけ)
        assert!(
            !home.join("config.toml.broken").exists(),
            "知らないキーがあるだけで「壊れた設定」にされた"
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **壊れた設定**: 1 文字のミスでも起動できて、控えが残る。
    #[test]
    fn 壊れた設定でも起動して控えが残る() {
        let home = crate::test_util::unique_temp_dir("zv-e2e", "broken");
        let root = crate::test_util::unique_temp_dir("zv-e2e", "broken-ws");
        std::fs::write(home.join("config.toml"), "theme = \"zaivern-dark\n[[[oops")
            .expect("write broken config");
        let cfg = boot_and_draw(&home, std::slice::from_ref(&root));
        assert!(!cfg.agents.is_empty(), "既定へ倒れていない");
        assert!(
            home.join("config.toml.broken").exists(),
            "壊れた設定の控えが残らない (直す手がかりが消える)"
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&root);
    }
}

// ── 層 1-e: 表示倍率 (DPI) ──────────────────────────────────────────────
//
// **なぜ要るか。** `theme::apply` は「その時点の `pixels_per_point` で
// 文字サイズと余白を物理ピクセルへ丸める」。つまりレイアウトの数字は
// **倍率ごとに違う**。上の層 1-a〜1-d は全部 ppp = 1.0 でしか回っていないので、
// Retina (2.0) や Windows の 125% (1.25) でだけ壊れる形は 1 つも見ていない。
//
// ここは「同じ検査を倍率だけ変えて通す」。**いちばん安い**穴埋めである
// (新しい検査を書かず、既にある検査を別の入力で回すだけ)。

/// 開いている窓が全部 `p.screen` の中にあり、掴めることを見る。
///
/// `dialog_tests` の中の助けと同じ判定を、`Painted` だけから行える形にした
/// (DPI / サブディスプレイの各モジュールから使う)。
#[cfg(test)]
fn assert_windows_inside(p: &Painted, what: &str) {
    let wins = p.windows();
    assert!(
        !wins.is_empty(),
        "{what}: 窓が 1 つも開かなかった (テストが何も見ていない)"
    );
    let named: Vec<(String, Rect)> = wins
        .iter()
        .map(|(l, r)| (format!("{what}/{:?}", l.order), *r))
        .collect();
    let bad = outside(p.screen, &named, 1.0);
    assert!(
        bad.is_empty(),
        "{what}: ダイアログが画面外へ出た:\n{}",
        bad.join("\n")
    );
    let bad = unclickable(p.screen, &named);
    assert!(
        bad.is_empty(),
        "{what}: 掴めない窓がある:\n{}",
        bad.join("\n")
    );
}

/// 「とても長いラベル」を持つパレットの候補 `n` 件。溢れの検査に使う。
#[cfg(test)]
fn long_palette_items(n: usize) -> Vec<crate::palette::Item> {
    (0..n)
        .map(|i| crate::palette::Item {
            icon: "📄".into(),
            label: format!("とても長いラベル {i} — src/very/deeply/nested/file_{i}.rs"),
            detail: format!("src/very/deeply/nested/path/to/a/file_{i}.rs"),
            action: crate::palette::Action::OpenFile(std::path::PathBuf::from("a.rs")),
            score: 0,
        })
        .collect()
}

/// 押し先を探せるよう短い名前を付けたパレットの候補 `n` 件。
#[cfg(test)]
fn named_palette_items(prefix: &str, n: usize) -> Vec<crate::palette::Item> {
    (0..n)
        .map(|i| crate::palette::Item {
            icon: "📄".into(),
            label: format!("{prefix}-{i}"),
            detail: format!("src/{prefix}_{i}.rs"),
            action: crate::palette::Action::OpenFile(std::path::PathBuf::from("a.rs")),
            score: 0,
        })
        .collect()
}

#[cfg(test)]
mod dpi_tests {
    use super::*;

    /// 実際に世の中に居る倍率。
    ///
    /// - 1.0 — Windows の 100% 表示 (端末セルのガタつきが最悪化する条件)
    /// - 1.25 / 1.5 / 1.75 — Windows の 125% / 150% / 175%
    /// - 1.3333333 — 「割り切れない」倍率。丸めの誤差が積もる形を必ず 1 つ入れる
    /// - 2.0 — macOS の Retina
    const PPPS: &[f32] = &[1.0, 1.25, 1.3333333, 1.5, 1.75, 2.0];

    /// 申告した倍率が**本当に効いている**。
    ///
    /// これが効いていないと、以下の DPI テストは全部「1.0 を 4 回」に
    /// なって静かに緑になる。土台の自己検査。
    #[test]
    fn 申告した倍率がcontextに届く() {
        for &ppp in PPPS {
            let mut s = Screen::new(800.0, 600.0).ppp(ppp);
            let (got, _) = s.settle(2, |ctx| ctx.pixels_per_point());
            assert!(
                (got - ppp).abs() < 1e-3,
                "ppp={ppp} を申告したのに ctx は {got} を返した (DPI テストが何も見ていない)"
            );
        }
    }

    /// 倍率を変えると**文字サイズの丸めも追随する**。
    ///
    /// `theme::apply` が積んだ `on_begin_pass` フック (`resync_pixel_snapping`)
    /// が回っている証拠。回っていなければ、丸めが 1.0 のままなので
    /// ppp = 1.25 / 1.5 で必ず端数が残る。
    #[test]
    fn 倍率を変えると文字サイズが物理ピクセルへ丸め直される() {
        for &ppp in PPPS {
            let mut s = Screen::new(800.0, 600.0).ppp(ppp);
            let (size_pt, _) =
                s.settle(3, |ctx| ctx.style().text_styles[&egui::TextStyle::Body].size);
            let px = size_pt * ppp;
            assert!(
                (px - px.round()).abs() < 1e-3,
                "ppp={ppp}: 本文の文字サイズ {size_pt}pt = {px}px が整数ピクセルでない"
            );
        }
    }

    /// **端末の桁は、どの倍率でも等間隔である。**
    ///
    /// CLAUDE.md の「端末セルは整数ピクセルに揃える」をそのまま検査にしたもの。
    /// `col * cell_w` を小数のまま使うと epaint は文字位置だけを丸めるので、
    /// 桁の間隔が 8/8/7/8 px と揺れて文字がガタガタに見える
    /// (100% 表示 = ppp 1.0 の Windows で最悪化する)。
    ///
    /// **絶対時間ではなく守りたい性質そのものを測る**: 隣り合う桁の物理
    /// ピクセル差が全部同じ値であること。`terminal::cell_metrics` の
    /// `snap_len` を外すと、この差がばらけて落ちる。
    #[test]
    fn どの倍率でも端末の桁は等間隔() {
        for &ppp in PPPS {
            for &size in &[1.0_f32, 6.0, 11.0, 12.0, 13.5, 14.0, 16.0, 24.0, 48.0] {
                let mut s = Screen::new(1200.0, 700.0).ppp(ppp);
                let ((cell_w, cell_h), _) = s.panel(Vec::new(), |ui| {
                    let (_, w, h) = crate::terminal::cell_metrics(ui, size);
                    (w, h)
                });
                assert!(
                    cell_w > 0.0 && cell_h > 0.0,
                    "ppp={ppp} size={size}: セルの寸法が 0"
                );
                let px: Vec<i64> = (0..40)
                    .map(|c| ((c as f32) * cell_w * ppp).round() as i64)
                    .collect();
                let gaps: Vec<i64> = px.windows(2).map(|w| w[1] - w[0]).collect();
                let first = gaps[0];
                assert!(
                    gaps.iter().all(|g| *g == first),
                    "ppp={ppp} size={size}: 桁の間隔が揃わない (cell_w={cell_w}) 先頭 8 個={:?}",
                    &gaps[..8]
                );
                // 行の高さも同じ性質を持つ (縦のリズムが崩れると行が重なって見える)。
                let hpx = cell_h * ppp;
                assert!(
                    (hpx - hpx.round()).abs() < 1e-3,
                    "ppp={ppp} size={size}: 行の高さ {cell_h}pt = {hpx}px が整数ピクセルでない"
                );
            }
        }
    }

    /// どの倍率でも**ダイアログは画面の外へ出ない**。
    #[test]
    fn どの倍率でもダイアログは画面内に収まる() {
        let theme = crate::theme::all()[0].clone();
        for &ppp in PPPS {
            for &(w, h) in SIZES {
                // 調停フォーム (いちばん背が高い) と エージェント追加 の 2 枚。
                let rows: Vec<crate::orchestration::SessionRow> = Vec::new();
                let owners = std::collections::BTreeMap::new();
                let mut s = Screen::new(w, h).ppp(ppp);
                let mut st = crate::orchestration::OrchState::default();
                st.form_open = true;
                let (_, p) = s.settle(3, |ctx| {
                    let _ =
                        crate::orchestration::task_form_ui(&mut st, ctx, &theme, &rows, &owners);
                });
                assert_windows_inside(&p, &format!("新しいタスク ppp={ppp} {w}x{h}"));

                let mut s = Screen::new(w, h).ppp(ppp);
                let mut picker = crate::agent_picker::AgentPicker::default();
                picker.open = true;
                let presets: Vec<crate::config::AgentPreset> = Vec::new();
                let (_, p) = s.settle(3, |ctx| {
                    let _ = crate::agent_picker::ui(&mut picker, ctx, &theme, &presets);
                });
                assert_windows_inside(&p, &format!("エージェント追加 ppp={ppp} {w}x{h}"));
            }
        }
    }

    /// どの倍率でも**行が見切れない**。
    #[test]
    fn どの倍率でも行が切れない() {
        let theme = crate::theme::all()[0].clone();
        let res = crate::palette::Palette::new().results(long_palette_items(8), &[]);
        for &ppp in PPPS {
            for &(w, h) in SIZES {
                let mut s = Screen::new(w, h).ppp(ppp);
                let (_, p) = s.panel(Vec::new(), |ui| {
                    crate::palette::list_ui(ui, &theme, &res, 0, false);
                });
                let bad = p.clipped_texts();
                assert!(
                    bad.is_empty(),
                    "パレット ppp={ppp} {w}x{h}: {} 行が可用幅から切れた:\n{}",
                    bad.len(),
                    bad.iter()
                        .take(5)
                        .map(|t| format!("  {:?} rect={:?} clip={:?}", t.text, t.rect, t.clip))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
        }
    }

    /// どの倍率でも**クリックが届く**。
    ///
    /// ポインタ位置は「点」で来るのに、当たり判定の矩形は物理ピクセルへ
    /// 丸められた寸法から組まれる。丸めが片側だけに効くと、**押した場所と
    /// 反応する場所がずれる**。
    #[test]
    fn どの倍率でもパレットの行が押せる() {
        let theme = crate::theme::all()[0].clone();
        let res = crate::palette::Palette::new().results(named_palette_items("zv-dpi", 6), &[]);
        for &ppp in PPPS {
            for &(w, h) in SIZES {
                let mut s = Screen::new(w, h).ppp(ppp);
                let (_, p) = s.settle(2, |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        crate::palette::list_ui(ui, &theme, &res, 0, false);
                    });
                });
                let pos = p.center_of("zv-dpi-2").unwrap_or_else(|| {
                    panic!(
                        "ppp={ppp} {w}x{h}: パレットの行が描かれていない\n{}",
                        p.joined()
                    )
                });
                let hit = s
                    .click_panel(pos, |ui| {
                        crate::palette::list_ui(ui, &theme, &res, 0, false)
                    })
                    .unwrap_or_else(|| {
                        panic!("ppp={ppp} {w}x{h}: 行 (zv-dpi-2) を押しても何も返らない")
                    });
                assert_eq!(
                    hit.label, "zv-dpi-2",
                    "ppp={ppp} {w}x{h}: 別の行が反応した"
                );
            }
        }
    }
}

// ── 層 1-f: IME (日本語 / ハングル入力) ─────────────────────────────────
//
// **なぜ最優先か。** CLAUDE.md の設計原則は「CJK/IME を最優先の品質領域と
// する」と書いている (競合の最大反応数の未修正バグがハングル分解と日本語入力)。
//
// **既にあるテストとの違い。** `terminal::translate_input` と
// `keybinds::ime_blocks_shortcuts` には表で固定した単体テストがあるが、
// どちらも **`&[egui::Event]` を直に関数へ渡している**。つまり
// 「そのイベントが `egui::Context` を通っても生き残るか」は誰も見ていない。
// これは絵空事ではない — egui-winit 0.29 は ⌘X / ⌘C / ⌘V の押下を
// `Event::Cut/Copy/Paste` へすり替えて**その場で return** するので、
// 「イベントは作ったのにアプリまで届かない」は**このリポジトリで実在する
// 壊れ方**である。ここは `ctx.run()` を通した後の姿だけを見る。

#[cfg(test)]
mod ime_tests {
    use super::*;

    fn enabled() -> egui::Event {
        egui::Event::Ime(egui::ImeEvent::Enabled)
    }
    fn preedit(s: &str) -> egui::Event {
        egui::Event::Ime(egui::ImeEvent::Preedit(s.into()))
    }
    fn commit(s: &str) -> egui::Event {
        egui::Event::Ime(egui::ImeEvent::Commit(s.into()))
    }
    fn disabled() -> egui::Event {
        egui::Event::Ime(egui::ImeEvent::Disabled)
    }
    fn key(k: egui::Key) -> egui::Event {
        egui::Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// **土台の自己検査**: `Event::Ime` は `ctx.run()` を通っても消えない。
    ///
    /// 消えるならこの下の IME テストは全部「何も流れていないのに緑」になる。
    /// ⌘V の押下が飲み込まれる前例があるので、思い込みでは済ませない。
    #[test]
    fn imeイベントはcontextを通ってもアプリまで届く() {
        let mut s = Screen::new(600.0, 200.0);
        let seq = vec![enabled(), preedit("にほんご"), commit("日本語"), disabled()];
        let (seen, _) = s.run(seq, |ctx| {
            ctx.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::Ime(x) => Some(format!("{x:?}")),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
        });
        assert_eq!(
            seen.len(),
            4,
            "IME イベントが Context を通る間に減った: {seen:?}"
        );
        assert!(seen[1].contains("にほんご"), "未確定文字列が化けた: {seen:?}");
        assert!(seen[2].contains("日本語"), "確定文字列が化けた: {seen:?}");
    }

    /// **変換中の未確定文字列は、確定するまで確定扱いにならない。**
    ///
    /// egui 0.29 の `TextEdit` は未確定文字列をその場に差し込み、確定時に
    /// 差し込んだぶんを消してから確定文字列を入れる (インライン変換)。
    /// 途中の姿は環境で違ってよいが、**終わった後の姿は 1 つしかない** —
    /// 確定文字列がちょうど 1 回だけ入っていること。
    /// ここが崩れると「にほんご日本語」や「日本語日本語」になる。
    #[test]
    fn 変換の一巡で確定文字列がちょうど一度だけ入る() {
        for (kana, kanji) in [
            ("にほんご", "日本語"),
            ("かんじ", "漢字"),
            // ハングルは 1 打鍵ごとに音節が組み上がる (競合が壊した経路)
            ("ㅎ", "한"),
        ] {
            let mut buf = String::new();
            let mut s = Screen::new(600.0, 200.0);
            let draw = |ctx: &egui::Context, buf: &mut String| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let r = ui.add(egui::TextEdit::singleline(buf).id_salt("zv-ime"));
                    r.request_focus();
                });
            };
            // 焦点を確定させるフレーム
            s.run(Vec::new(), |ctx| draw(ctx, &mut buf));
            // 変換を開始 → 未確定を伸ばす → 確定 → 閉じる
            for step in [
                vec![enabled()],
                vec![preedit(&kana[..kana.char_indices().nth(1).map_or(kana.len(), |(i, _)| i)])],
                vec![preedit(kana)],
                vec![preedit(""), commit(kanji)],
                vec![disabled()],
            ] {
                s.run(step, |ctx| draw(ctx, &mut buf));
            }
            assert_eq!(
                buf, kanji,
                "変換が一巡した後の中身が確定文字列と違う (未確定が残ったか二重に入った)"
            );
        }
    }

    /// **変換中は、アプリのショートカットが発火しない。**
    ///
    /// これが崩れると「ひらがなを打っているのにコマンドが走る」
    /// 「変換確定の Enter がコマンドとして食われて確定できない」になる。
    ///
    /// **わざと壊して落ちることも同時に見る** — 同じイベントを
    /// `keybinds::ime_blocks_shortcuts_now` の関門**なし**で流すと、
    /// Enter は必ず消費される。関門が効いている証拠を A/B で残す。
    #[test]
    fn 変換中の打鍵はアプリのショートカットに奪われない() {
        // 関門を通す版 / 通さない版。どちらも状態機械は 1 フレーム 1 回進める。
        fn frame(ctx: &egui::Context, gated: bool) -> bool {
            let blocked = crate::keybinds::ime_blocks_shortcuts_now(ctx);
            if gated && blocked {
                return false;
            }
            ctx.input_mut(|i| {
                crate::keybinds::consume_shortcut_compat(
                    i,
                    egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::Enter),
                )
            })
        }

        // 変換の並びは環境で違う。macOS は Disabled を出さず、Windows は
        // Commit の前後に Enabled/Disabled を出す。**両方**で見る。
        for (name, steps) in [
            (
                "macOS 風 (Disabled 無し)",
                vec![
                    vec![enabled()],
                    vec![preedit("にほん")],
                    vec![preedit("にほんご")],
                    // 確定の Enter は同じフレームに載る
                    vec![key(egui::Key::Enter), preedit(""), commit("日本語")],
                ],
            ),
            (
                "Windows 風 (Enabled/Disabled あり)",
                vec![
                    vec![enabled()],
                    vec![preedit("かんじ")],
                    vec![key(egui::Key::Enter), commit("漢字")],
                    vec![disabled()],
                ],
            ),
            (
                "ハングル (1 打鍵ごとに確定)",
                vec![
                    vec![enabled()],
                    vec![preedit("ㅎ")],
                    vec![preedit("하")],
                    vec![key(egui::Key::Enter), preedit(""), commit("한")],
                ],
            ),
        ] {
            let mut s = Screen::new(600.0, 200.0);
            for step in steps.clone() {
                let (fired, _) = s.run(step, |ctx| frame(ctx, true));
                assert!(
                    !fired,
                    "{name}: 変換中に Enter のバインドが発火した (確定できない / コマンドが暴発する)"
                );
            }
            // 変換が閉じた**次の**フレームの Enter は通常どおり効く
            // (「確定 → 送信」の 2 打鍵が成立しなければ日本語入力は使えない)。
            let (fired, _) = s.run(vec![key(egui::Key::Enter)], |ctx| frame(ctx, true));
            assert!(fired, "{name}: 変換が終わった後の Enter まで殺している");

            // ── わざと壊す: 関門を外すと同じ並びで必ず暴発する ──
            let mut s = Screen::new(600.0, 200.0);
            let mut fired_any = false;
            for step in steps {
                let (fired, _) = s.run(step, |ctx| frame(ctx, false));
                fired_any |= fired;
            }
            assert!(
                fired_any,
                "{name}: 関門を外しても暴発しない = このテストは関門を見ていない"
            );
        }
    }

    /// 変換中フラグは**フレームをまたいで持ち越される**。
    ///
    /// `ime_blocks_shortcuts_now` は `ctx.data` に状態を置く。`Context` を
    /// 通さない単体テストでは、この持ち越しは 1 バイトも動かない。
    #[test]
    fn 変換中フラグはフレームをまたいで生き残る() {
        let mut s = Screen::new(400.0, 200.0);
        // 変換を開始したフレーム
        let (blocked, _) = s.run(vec![preedit("にほ")], |ctx| {
            crate::keybinds::ime_blocks_shortcuts_now(ctx)
        });
        assert!(blocked, "未確定文字列が出たフレームで止まっていない");
        // **IME イベントが 1 つも無いフレーム**でも、まだ変換中である
        for n in 0..3 {
            let (blocked, _) = s.run(Vec::new(), |ctx| {
                crate::keybinds::ime_blocks_shortcuts_peek(ctx)
            });
            assert!(blocked, "{n} フレーム後に変換中フラグが消えた");
        }
        // 空の Preedit が来たら変換は閉じる
        s.run(vec![preedit("")], |ctx| {
            crate::keybinds::ime_blocks_shortcuts_now(ctx)
        });
        let (blocked, _) = s.run(Vec::new(), |ctx| {
            crate::keybinds::ime_blocks_shortcuts_peek(ctx)
        });
        assert!(!blocked, "変換を閉じたのに止めたままになっている");
    }

    /// 変換していないフレームは **1 回も止めない**。
    ///
    /// 「常に止める」で上のテストは全部通ってしまうので、必ず対にする。
    #[test]
    fn 変換していないフレームはショートカットを止めない() {
        let mut s = Screen::new(400.0, 200.0);
        for _ in 0..5 {
            let (blocked, _) = s.run(vec![key(egui::Key::Enter)], |ctx| {
                crate::keybinds::ime_blocks_shortcuts_now(ctx)
            });
            assert!(!blocked, "変換していないのにショートカットを止めた");
        }
    }

    /// **CJK の未確定文字列は、桁の幅ぶんちょうどを占める。**
    ///
    /// 端末のオーバーレイ幅は `textenc::str_width(preedit) * cell_w`
    /// (`terminal.rs`)。全角は 2 桁なので、ここが 1 桁で数えられていると
    /// 未確定文字列が後ろの本文に**重なる**。どの倍率でも成り立つこと、
    /// そして重ねて描いても端末の矩形からはみ出さないことまで見る。
    #[test]
    fn どの倍率でもcjkの未確定文字列は桁の幅ぴったりを占める() {
        for &ppp in &[1.0_f32, 1.25, 1.5, 2.0] {
            let mut s = Screen::new(1000.0, 600.0).ppp(ppp);
            let ((cell_w, widths), _) = s.panel(Vec::new(), |ui| {
                let (_, w, _) = crate::terminal::cell_metrics(ui, 13.0);
                let widths: Vec<(&str, usize)> = ["日本語", "한글", "abc", "ｱｲｳ", "🙂"]
                    .iter()
                    .map(|t| (*t, crate::textenc::str_width(t)))
                    .collect();
                (w, widths)
            });
            let expect = [("日本語", 6), ("한글", 4), ("abc", 3), ("ｱｲｳ", 3), ("🙂", 2)];
            for ((t, got), (_, want)) in widths.iter().zip(expect.iter()) {
                assert_eq!(got, want, "ppp={ppp}: {t:?} の桁数が {got} (期待 {want})");
            }
            // 端末の矩形 (幅 1000pt − 余白) に収まる桁数を超えないこと
            let cols = ((1000.0 - 8.0) / cell_w).floor();
            assert!(
                cols >= 6.0,
                "ppp={ppp}: 1000pt の端末に 6 桁も入らない (cell_w={cell_w})"
            );
        }
    }
}

// ── 層 1-g: サブディスプレイ (画面の原点が (0,0) でない) とフォーカス ────
//
// **なぜ要るか。** 上の層は全部 `screen_rect` の左上を原点に置いている。
// 実際には macOS は主ディスプレイの左上を 0 とするので、左や上に置いた
// サブディスプレイの窓は**負の座標**を持つ。`avail.width()` から寸法を
// 出しておきながら位置を `Pos2::ZERO` 起点で組むコードは、そこで初めて壊れる。
//
// CLAUDE.md にはこの向きの実害が既に記録されている —
// 「縦オフセット配置のサブディスプレイでネイティブ全画面にすると、
// 当たり判定が素の座標のままなので見えている場所を押しても効かない」。

#[cfg(test)]
mod subdisplay_tests {
    use super::*;

    /// 実際にありうる置き方。macOS は左/上を負で表す。
    const ORIGINS: &[(f32, f32)] = &[
        (0.0, 0.0),         // 主ディスプレイ
        (-1920.0, 0.0),     // 左に 1 枚
        (-1440.0, -300.0),  // 左上に、縦をずらして置いた (fullscreen_guard の実害の形)
        (2560.0, 400.0),    // 右に、縦をずらして置いた
        (0.0, -1080.0),     // 上に 1 枚
    ];

    /// **土台の自己検査**: 申告した原点が `ctx.screen_rect()` に届く。
    #[test]
    fn 申告した画面の原点がcontextに届く() {
        for &(x, y) in ORIGINS {
            let mut s = Screen::new(900.0, 700.0).at(egui::pos2(x, y));
            let (got, _) = s.settle(2, |ctx| ctx.screen_rect());
            assert!(
                (got.min - egui::pos2(x, y)).length() < 0.5,
                "原点 ({x},{y}) を申告したのに ctx は {got:?} を返した"
            );
        }
    }

    /// どの原点でも**ダイアログは画面の中**に居る。
    #[test]
    fn 原点がずれた画面でもダイアログは画面内に収まる() {
        let theme = crate::theme::all()[0].clone();
        for &(x, y) in ORIGINS {
            for &(w, h) in SIZES {
                let at = egui::pos2(x, y);

                let mut s = Screen::new(w, h).at(at);
                let mut picker = crate::agent_picker::AgentPicker::default();
                picker.open = true;
                let presets: Vec<crate::config::AgentPreset> = Vec::new();
                let (_, p) = s.settle(3, |ctx| {
                    let _ = crate::agent_picker::ui(&mut picker, ctx, &theme, &presets);
                });
                assert_windows_inside(&p, &format!("エージェント追加 @({x},{y}) {w}x{h}"));

                let mut s = Screen::new(w, h).at(at);
                let radar = crate::conflict::ConflictRadar::default();
                let mut open = true;
                let mut sel = None;
                let (_, p) = s.settle(3, |ctx| {
                    let _ = crate::conflict::radar_window(ctx, &theme, &mut open, &radar, &mut sel);
                });
                assert_windows_inside(&p, &format!("衝突レーダー @({x},{y}) {w}x{h}"));
            }
        }
    }

    /// どの原点でも**クリックが届く**。
    ///
    /// 「見えている場所を押しても効かない」の再現条件そのもの。
    #[test]
    fn 原点がずれた画面でもパレットの行が押せる() {
        let theme = crate::theme::all()[0].clone();
        let res = crate::palette::Palette::new().results(named_palette_items("zv-sub", 6), &[]);
        for &(x, y) in ORIGINS {
            for &(w, h) in SIZES {
                let mut s = Screen::new(w, h).at(egui::pos2(x, y));
                let (_, p) = s.settle(2, |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        crate::palette::list_ui(ui, &theme, &res, 0, false);
                    });
                });
                let pos = p.center_of("zv-sub-2").unwrap_or_else(|| {
                    panic!("@({x},{y}) {w}x{h}: 行が描かれていない\n{}", p.joined())
                });
                let hit = s
                    .click_panel(pos, |ui| {
                        crate::palette::list_ui(ui, &theme, &res, 0, false)
                    })
                    .unwrap_or_else(|| {
                        panic!("@({x},{y}) {w}x{h}: 行を押しても何も返らない (座標が原点前提)")
                    });
                assert_eq!(hit.label, "zv-sub-2", "@({x},{y}) {w}x{h}: 別の行が反応した");
            }
        }
    }

    /// どの原点でも**行が見切れない**。
    #[test]
    fn 原点がずれた画面でも行が切れない() {
        let theme = crate::theme::all()[0].clone();
        let res = crate::palette::Palette::new().results(long_palette_items(8), &[]);
        for &(x, y) in ORIGINS {
            for &(w, h) in SIZES {
                let mut s = Screen::new(w, h).at(egui::pos2(x, y));
                let (_, p) = s.panel(Vec::new(), |ui| {
                    crate::palette::list_ui(ui, &theme, &res, 0, false);
                });
                let bad = p.clipped_texts();
                assert!(
                    bad.is_empty(),
                    "パレット @({x},{y}) {w}x{h}: {} 行が切れた",
                    bad.len()
                );
            }
        }
    }

    /// **レイアウトの純関数は、画面をまるごと平行移動しても同じ形を返す。**
    ///
    /// `f(領域 + d) == f(領域) + d` (平行移動同変)。これが成り立たない関数は
    /// 「幅は `avail.width()` から出しているのに位置は `Pos2::ZERO` 起点」
    /// という形をしており、サブディスプレイでだけ矩形が別の場所へ飛ぶ。
    /// **わざと壊すと捕まる**ことは `平行移動の検査はわざと壊すと訴える` で固定する。
    #[test]
    fn レイアウトの純関数は画面の原点に依らない() {
        let deltas = [
            egui::vec2(0.0, 0.0),
            egui::vec2(-1920.0, 0.0),
            egui::vec2(-1440.0, -300.0),
            egui::vec2(2560.0, 400.0),
        ];
        for &(w, h) in SIZES {
            let base = Rect::from_min_size(Pos2::ZERO, egui::vec2(w, h));
            for d in deltas {
                let moved = base.translate(d);
                for presets in 0..8usize {
                    let a = crate::panels::empty_card(base, presets).card.translate(d);
                    let b = crate::panels::empty_card(moved, presets).card;
                    assert_rect_eq(a, b, &format!("empty_card {w}x{h} presets={presets} d={d:?}"));
                }
                for rows in 0..6usize {
                    for buttons in 0..5usize {
                        let a = crate::panels::media_card(base, rows, buttons).card.translate(d);
                        let b = crate::panels::media_card(moved, rows, buttons).card;
                        assert_rect_eq(
                            a,
                            b,
                            &format!("media_card {w}x{h} rows={rows} btn={buttons} d={d:?}"),
                        );
                    }
                }
                for ratio in [0.0_f32, 0.1, 0.45, 0.9, 1.0] {
                    let a = crate::marks::panel_layout(base, ratio);
                    let b = crate::marks::panel_layout(moved, ratio);
                    for (name, ra, rb) in [
                        ("tree", a.tree, b.tree),
                        ("splitter", a.splitter, b.splitter),
                        ("preview", a.preview, b.preview),
                    ] {
                        assert_rect_eq(
                            ra.translate(d),
                            rb,
                            &format!("panel_layout/{name} {w}x{h} ratio={ratio} d={d:?}"),
                        );
                    }
                }
            }
        }
    }

    /// 平行移動の検査そのものが、**ずれた入力で必ず訴える**ことを固定する。
    #[test]
    fn 平行移動の検査はわざと壊すと訴える() {
        let a = Rect::from_min_size(Pos2::ZERO, egui::vec2(10.0, 10.0));
        assert!(rect_diff(a, a).is_none(), "同じ矩形を違うと言った");
        let b = a.translate(egui::vec2(2.0, 0.0));
        assert!(rect_diff(a, b).is_some(), "2px のずれを見逃した");
        // 「原点起点で組んでしまった」形 (幅は正しいのに位置が取り残される) も捕まる
        assert!(
            rect_diff(a.translate(egui::vec2(-1920.0, 0.0)), a).is_some(),
            "原点起点の取り残しを見逃した"
        );
    }

    /// **フォーカスを失っても描画は壊れず、戻せば押せる。**
    ///
    /// `⌘Tab` で裏へ回っている間に描画が空になったり落ちたりしないこと、
    /// そして戻ってきたフレームで当たり判定が生きていること。
    #[test]
    fn フォーカスを失っても描けて戻せば押せる() {
        let theme = crate::theme::all()[0].clone();
        let res = crate::palette::Palette::new().results(named_palette_items("zv-focus", 4), &[]);

        // 裏に回っている間
        let mut s = Screen::new(900.0, 700.0).focus(false);
        let (_, p) = s.settle(3, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                crate::palette::list_ui(ui, &theme, &res, 0, false);
            });
        });
        assert!(
            p.find("zv-focus-2").is_some(),
            "フォーカスを失うと描画が消えた\n{}",
            p.joined()
        );
        assert!(
            !s.ctx.input(|i| i.focused),
            "フォーカス無しを申告したのに ctx は focused のまま"
        );

        // 戻ってきたら押せる
        let mut s = Screen::new(900.0, 700.0).focus(true);
        let (_, p) = s.settle(2, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                crate::palette::list_ui(ui, &theme, &res, 0, false);
            });
        });
        let pos = p.center_of("zv-focus-2").expect("行が無い");
        let hit = s
            .click_panel(pos, |ui| {
                crate::palette::list_ui(ui, &theme, &res, 0, false)
            })
            .expect("フォーカスを取り戻しても押せない");
        assert_eq!(hit.label, "zv-focus-2");
    }

    /// **モニタ実寸と全画面フラグが `ViewportInfo` として届く。**
    ///
    /// `app::fullscreen_guard` は `i.viewport()` の
    /// `fullscreen` / `inner_rect` / `monitor_size` の 3 つだけを見て
    /// 「窓がモニタより大きい (= 壊れたネイティブ全画面)」を判定する。
    /// ここでその 3 つを**任意の値で流し込める**ことを固定しておくと、
    /// `ZaivernApp` が組めるようになった時点で判定そのものを回せる
    /// (現状 `fullscreen_guard` は `src/app/` の中で `&mut self` を取るため
    ///  ここからは呼べない — 詳しくは `app_reachability_tests`)。
    #[test]
    fn 壊れた全画面の入力条件をそのまま流し込める() {
        // 縦 1080 のモニタに、縦 1120 の窓 (= メインとの配置差 40px ぶん過大)。
        let mut s = Screen::new(1920.0, 1120.0)
            .at(egui::pos2(0.0, -1080.0))
            .monitor(egui::vec2(1920.0, 1080.0))
            .fullscreen(true);
        let (got, _) = s.settle(2, |ctx| {
            ctx.input(|i| {
                let v = i.viewport();
                (v.fullscreen, v.inner_rect, v.monitor_size)
            })
        });
        let (fs, inner, mon) = got;
        assert_eq!(fs, Some(true), "全画面フラグが届かない");
        let inner = inner.expect("窓の矩形が届かない");
        let mon = mon.expect("モニタ実寸が届かない");
        assert!(
            inner.height() > mon.y + 1.0,
            "この入力は「窓 > モニタ」でなければ意味が無い (窓 {inner:?} / モニタ {mon:?})"
        );
    }
}

/// 2 つの矩形のずれ (無ければ `None`)。許容は 0.5px。
#[cfg(test)]
fn rect_diff(a: Rect, b: Rect) -> Option<String> {
    if (a.min - b.min).length() <= 0.5 && (a.max - b.max).length() <= 0.5 {
        None
    } else {
        Some(format!("{a:?} != {b:?}"))
    }
}

#[cfg(test)]
fn assert_rect_eq(a: Rect, b: Rect, what: &str) {
    if let Some(d) = rect_diff(a, b) {
        panic!("{what}: 画面を平行移動したのに矩形がついてこない: {d}");
    }
}

// ── 「アプリ全体を E2E に載せられるか」の結論と、その番人 ────────────────
//
// 前任者は「`ZaivernApp` は `eframe::CreationContext` の `pub(crate)`
// フィールドのせいでテストから作れない」と書いた。**これは正しい**が、
// 正しいのは *eframe の型が外から組めない* ところまでで、
// 「だから全体 E2E は不可能」までは言えない。実際に閉ざしているのは
// 次の **2 点だけ**である (eframe 0.29.1 の実物で確認):
//
// 1. `eframe::CreationContext` — `raw_window_handle` /
//    `raw_display_handle` が `pub(crate)`。公開の構築子も `Default` も無い。
//    → **外部クレートからは絶対に組めない。**
// 2. `eframe::Frame` — フィールドが**全部** `pub(crate)`。同上。
//    `eframe::App::update(&mut self, ctx, frame: &mut eframe::Frame)` は
//    この型を要求するので、`update` は外から呼べない。
//
// **どちらもこのリポジトリ側の 2 行で回避できる。** 実測した根拠:
//
// * `ZaivernApp::new` が `cc` から読むのは **`cc.egui_ctx` だけ** (9 箇所)。
//   `cc.storage` / `cc.gl` / `cc.wgpu_render_state` は 1 度も触っていない。
//   → 引数を `&egui::Context` にした `new_in_ctx` を切り出し、
//     `new(cc, …)` はそれへ委譲するだけでよい。
// * `update_impl` の第 2 引数は `_frame` — **先頭のアンダースコアどおり
//   本体で 1 度も使っていない**。→ 引数ごと落とせる。
//
// この 2 つを入れれば `Screen` の上で `ZaivernApp` をまるごと回せる。
// ただし `src/app/` は**別のエージェントが担当中**なので、ここでは触らない。
// 代わりに **前提が崩れたら落ちる番人**を置く — 誰かが `cc.storage` を
// 読み始めたり `_frame` を使い始めたら、この結論は無効になるからである。

#[cfg(test)]
mod app_reachability_tests {
    /// `ZaivernApp::new` が `CreationContext` から読むのは `egui_ctx` だけ。
    ///
    /// ここが崩れると「引数を `&egui::Context` に替えるだけ」では済まなくなり、
    /// 全体 E2E への道が閉じる。**改行は正規化する** (Windows のチェックアウトは CRLF)。
    #[test]
    fn 起動処理がcreationcontextから読むのはegui_ctxだけ() {
        let src = include_str!("app/startup.rs").replace("\r\n", "\n");
        let mut others: Vec<String> = Vec::new();
        for (i, line) in src.lines().enumerate() {
            let mut rest = line;
            while let Some(at) = rest.find("cc.") {
                let tail = &rest[at + 3..];
                let field: String = tail
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !field.is_empty() && field != "egui_ctx" {
                    others.push(format!("{}行目: cc.{field}", i + 1));
                }
                rest = &rest[at + 3..];
            }
        }
        assert!(
            others.is_empty(),
            "ZaivernApp::new が egui_ctx 以外の CreationContext を読み始めた。\n\
             全体 E2E への道 (new_in_ctx への切り出し) が閉じるので、\n\
             src/e2e.rs の app_reachability_tests の説明を書き直すこと:\n{}",
            others.join("\n")
        );
        assert!(
            src.contains("cc.egui_ctx"),
            "前提が崩れている: startup.rs に cc.egui_ctx が 1 つも無い"
        );
    }

    /// `update_impl` は `eframe::Frame` を**使っていない**。
    ///
    /// 使い始めたら `eframe::Frame` (全フィールド `pub(crate)`) が要るので、
    /// ヘッドレスからは二度と呼べなくなる。
    #[test]
    fn 毎フレーム処理はeframe_frameを使っていない() {
        let src = include_str!("app/frame_update.rs").replace("\r\n", "\n");
        assert!(
            src.contains("fn update_impl(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame)"),
            "update_impl の形が変わった。frame を使い始めていないか確かめ、\n\
             src/e2e.rs の app_reachability_tests の説明を書き直すこと"
        );
        // 本体で `_frame` を読み出していないこと (宣言の 1 回だけ)。
        // **単語境界で数える** — `perf_frame` のような別の識別子まで
        // 部分一致で拾うと、直っていないのに落ちる (実際に誤検出した)。
        let bytes = src.as_bytes();
        let uses = src
            .match_indices("_frame")
            .filter(|(i, _)| {
                *i == 0 || !(bytes[i - 1] as char).is_alphanumeric() && bytes[i - 1] != b'_'
            })
            .count();
        assert_eq!(
            uses, 1,
            "update_impl が _frame を使い始めた ({uses} 箇所)。\n\
             eframe::Frame は外から組めないので、全体 E2E への道が閉じる"
        );
    }

    /// **層 1 の土台は、アプリ本体を載せる準備ができている。**
    ///
    /// `ZaivernApp` を 1 行でも組めるようになれば、必要な入力
    /// (画面矩形 / 倍率 / フォーカス / モニタ実寸 / 全画面 / ポインタ / IME)
    /// は全部 `Screen` から流し込める — それを 1 本のフレームで示す。
    /// ここが緑である限り、残っている障害は **`src/app/` の 2 行だけ**である。
    #[test]
    fn 土台はアプリ本体を載せる入力を全部持っている() {
        let mut s = super::Screen::new(1280.0, 800.0)
            .ppp(2.0)
            .at(egui::pos2(-1920.0, -300.0))
            .monitor(egui::vec2(1920.0, 1080.0))
            .fullscreen(false)
            .focus(true);
        let (seen, painted) = s.run(
            vec![
                egui::Event::PointerMoved(egui::pos2(-1900.0, -280.0)),
                egui::Event::Ime(egui::ImeEvent::Preedit("にほんご".into())),
            ],
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label("ZV-READY");
                });
                // **`ctx.input()` の中で `ctx.*` を呼んではいけない。**
                // egui 0.29 の `Context::input` は `Context::write`
                // (RwLock の**排他**ロック) を取るので、中から
                // `ctx.pixels_per_point()` を呼ぶと自分自身を待って固まる
                // (実測: `parking_lot::RawRwLock::lock_exclusive_slow` で永久停止)。
                // 欲しい値は全部 `InputState` から取れる。
                ctx.input(|i| {
                    let v = i.viewport();
                    (
                        i.pixels_per_point(),
                        i.screen_rect.min,
                        i.focused,
                        v.monitor_size,
                        v.fullscreen,
                        i.pointer.latest_pos(),
                        i.events.iter().any(|e| matches!(e, egui::Event::Ime(_))),
                    )
                })
            },
        );
        let (ppp, origin, focused, mon, fs, ptr, ime) = seen;
        assert_eq!(ppp, 2.0, "倍率が届かない");
        assert_eq!(origin, egui::pos2(-1920.0, -300.0), "画面の原点が届かない");
        assert!(focused, "フォーカスが届かない");
        assert_eq!(mon, Some(egui::vec2(1920.0, 1080.0)), "モニタ実寸が届かない");
        assert_eq!(fs, Some(false), "全画面フラグが届かない");
        assert_eq!(ptr, Some(egui::pos2(-1900.0, -280.0)), "ポインタが届かない");
        assert!(ime, "IME イベントが届かない");
        assert!(painted.find("ZV-READY").is_some(), "描画が拾えない");
    }
}

// ── 実測で見つかった「まだ直っていない壊れ方」の再現 ────────────────────
//
// **`#[ignore]` にしてあるのは、直っていないからである。** 緑にするために
// 現状の (壊れた) 値を期待値へ書くと「壊れているのに緑」になるので、
// アサーションは**直った姿**のまま置き、既定の実行からは外してある。
//   再現: `cargo test --bin zai e2e::known_breakage -- --ignored --nocapture`

#[cfg(test)]
mod known_breakage {
    use super::*;

    /// **確定文字列が `Text` としても届く環境では、CJK が二重に入る。**
    ///
    /// `terminal::translate_input` はこれを規則 4 として明示的に潰している
    /// (「確定と同じ文字列が同フレームで `Text` としても届く環境がある。
    ///  そのまま流すと CJK が二重に入る (Windows の一部 IME で起きる)」)。
    /// **エディタ側にはその防御が無い。**
    ///
    /// 経路も確認済み: `egui-winit-0.29.1/src/lib.rs:786-800` の `Event::Text`
    /// は `WindowEvent::KeyboardInput` から出ており、IME の `Commit`
    /// (同 358-363 行) とは**互いに何も知らない**。両方来たら両方入る。
    ///
    /// 実測 (`Screen` + 素の `egui::TextEdit`):
    ///   `Commit("日本語")` + `Text("日本語")` を同フレーム → `"日本語日本語"`
    ///
    /// 影響範囲は端末ではなく**アプリのほぼ全ての入力欄** —
    /// コマンドパレットの検索欄 (`app/cmd_palette.rs`)、
    /// 検索・置換 (`app/editor_layout.rs`)、コードエディタ本体
    /// (`app/code_editor.rs` の `TextEdit::multiline`) が同じ経路を通る。
    ///
    /// **直し方 (1 箇所)**: `keybinds::ime_blocks_shortcuts_now` は
    /// `app::handle_shortcuts` の先頭 (`app/frame_update.rs:143`) で
    /// **1 フレームに 1 回**、しかも**パネルを描く前に**呼ばれている。
    /// そこで「同フレームの `Commit` と同じ文字列の `Text`」を
    /// `ctx.input_mut(|i| i.events.retain(..))` で 1 つだけ落とせばよい
    /// (端末の規則 4 と同じ判定)。`feature::draw_all` は
    /// `app/frame_update.rs:597` = 入力欄を描き終えた**後**なので使えない。
    ///
    /// `src/keybinds.rs` と `src/app/` はこの担当の管轄外なので、
    /// ここでは**再現だけ**を残す。
    #[test]
    #[ignore = "未修正の不具合の再現 (直し方は doc コメント)"]
    fn 確定文字列がtextとしても届くとcjkが二重に入る() {
        for (kana, kanji) in [("にほんご", "日本語"), ("한", "한")] {
            let mut buf = String::new();
            let mut s = Screen::new(600.0, 200.0);
            let draw = |ctx: &egui::Context, buf: &mut String| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add(egui::TextEdit::singleline(buf).id_salt("zv-dup"))
                        .request_focus();
                });
            };
            s.run(Vec::new(), |ctx| draw(ctx, &mut buf));
            s.run(vec![egui::Event::Ime(egui::ImeEvent::Enabled)], |ctx| {
                draw(ctx, &mut buf)
            });
            s.run(
                vec![egui::Event::Ime(egui::ImeEvent::Preedit(kana.into()))],
                |ctx| draw(ctx, &mut buf),
            );
            s.run(
                vec![
                    egui::Event::Ime(egui::ImeEvent::Preedit("".into())),
                    egui::Event::Ime(egui::ImeEvent::Commit(kanji.into())),
                    // 確定と同じ文字列が Text としても届く (Windows の一部 IME)
                    egui::Event::Text(kanji.into()),
                ],
                |ctx| draw(ctx, &mut buf),
            );
            assert_eq!(
                buf, kanji,
                "確定文字列が二重に入った (実測: {buf:?})。\n\
                 端末の規則 4 と同じ防御がエディタ側に無い"
            );
        }
    }
}
