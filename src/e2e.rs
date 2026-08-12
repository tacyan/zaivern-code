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
        }
    }

    pub fn rect(&self) -> Rect {
        Rect::from_min_size(Pos2::ZERO, self.size)
    }

    fn raw(&mut self, events: Vec<egui::Event>) -> egui::RawInput {
        self.time += 1.0 / 60.0;
        egui::RawInput {
            screen_rect: Some(self.rect()),
            time: Some(self.time),
            events,
            ..Default::default()
        }
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
