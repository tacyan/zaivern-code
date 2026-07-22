//! ペット吹き出しカード（clawd-on-desk 風のパーミッション承認バブル）
//!
//! ターミナルセッションが承認待ちのとき、ペット（またはスクリーン右下）から
//! 浮かび上がるカードを描画する。カードには [✔ 承認] [✖ 拒否] [開く] [×] の
//! ボタンが並び、クリック結果は `BubbleAction` として呼び出し側へ返す。
//!
//! 完全にステートレス: 呼び出し側が毎フレーム表示対象 `items` を絞り込み、
//! 返ってきたアクションを処理する。本モジュールは状態を一切保持しない
//! （入場アニメーションのみ egui の temp データを利用）。

use eframe::egui;

use crate::i18n::{tr, trf};

// ── レイアウト定数 ─────────────────────────────────────────────

/// カードの幅
const CARD_W: f32 = 260.0;
/// カード 1 枚分の高さ（積み上げ計算用の見積もり値）
const CARD_H: f32 = 70.0;
/// カード同士の縦間隔
const GAP: f32 = 8.0;
/// 画面端からの余白
const MARGIN: f32 = 12.0;
/// 左端アクセントストライプの幅
const STRIPE_W: f32 = 4.0;
/// 同時に表示するカードの最大枚数
const MAX_CARDS: usize = 3;
/// タイトルの最大文字数（超過分は … で省略）
const TITLE_MAX_CHARS: usize = 28;
/// "+他N件" ラベルの高さ（積み上げ計算用）
const MORE_LABEL_H: f32 = 18.0;
/// 入場アニメーションの時間（秒）
const ANIM_TIME: f32 = 0.22;
/// 入場アニメーションのスライド量（px）
const ANIM_SLIDE: f32 = 10.0;

// ── 公開 API ──────────────────────────────────────────────────

/// 吹き出しカード 1 枚分の表示内容
pub struct BubbleItem {
    /// 対象セッションのインデックス（アクション返却用。毎フレーム再構築される）
    pub session_idx: usize,
    /// セッションの安定 ID。egui の Id / 入場アニメの状態キーに使う
    /// （index はセッション削除で前へ詰まるため、キーには使わない）
    pub key: u64,
    /// カード左端に表示するアイコン（絵文字など）
    pub icon: String,
    /// カードのタイトル（長い場合は省略表示）
    pub title: String,
}

/// カード上のボタン操作の結果（usize = session_idx）
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum BubbleAction {
    /// ✔ 承認
    Approve(usize),
    /// ✖ 拒否
    Deny(usize),
    /// 開く（セッションへフォーカス移動）
    Focus(usize),
    /// × 吹き出しを閉じる
    Dismiss(usize),
}

/// 吹き出しカードのスタックを描画し、クリックされたアクションを返す。
///
/// - `anchor` が `Some` のときはその点（ペットの頭上中央）から上方向へ積む。
/// - `None` のときは画面右下コーナーから上方向へ積む。
/// - `items` が空なら何も描かず空の Vec を返す。
pub fn draw(
    ctx: &eframe::egui::Context,
    theme: &crate::theme::Theme,
    items: &[BubbleItem],
    anchor: Option<eframe::egui::Pos2>,
) -> Vec<BubbleAction> {
    let mut actions = Vec::new();
    if items.is_empty() {
        return actions;
    }

    let screen = ctx.screen_rect();
    let visible = items.len().min(MAX_CARDS);
    let overflow = items.len().saturating_sub(MAX_CARDS);
    // ── オーバーフロー時はスタック最下段に "+他N件" ラベル分の高さを確保
    let label_h = if overflow > 0 { MORE_LABEL_H + GAP } else { 0.0 };

    // ── スタックの基準位置（カード左端 x とスタック最下端 y）を決める
    let (base_x, base_bottom) = match anchor {
        // ペットの頭上中央から上方向へ
        Some(p) => (p.x - CARD_W * 0.5, p.y - GAP),
        // 画面右下コーナーから上方向へ
        None => (screen.right() - MARGIN - CARD_W, screen.bottom() - MARGIN),
    };

    // ── カード本体を描画（i = 0 がスタック最下段 = アンカーに最も近い）
    for (i, item) in items.iter().take(visible).enumerate() {
        let y = base_bottom - label_h - (i as f32 + 1.0) * CARD_H - i as f32 * GAP;
        let pos = clamp_to_screen(egui::pos2(base_x, y), screen, CARD_W, CARD_H);
        draw_card(ctx, theme, item, pos, &mut actions);
    }

    // ── あふれた件数の表示（スタックの下側に小さく）
    if overflow > 0 {
        let y = base_bottom - MORE_LABEL_H;
        let pos = clamp_to_screen(egui::pos2(base_x, y), screen, CARD_W, MORE_LABEL_H);
        egui::Area::new(egui::Id::new("zv-pet-bubble-more"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .interactable(false)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(trf("+他{overflow}件", &[("overflow", overflow.to_string())]))
                        .small()
                        .color(theme.text_dim),
                );
            });
    }

    actions
}

// ── 内部実装 ──────────────────────────────────────────────────

/// カードを 1 枚描画する。ボタンのクリックは `actions` へ追加される。
fn draw_card(
    ctx: &egui::Context,
    theme: &crate::theme::Theme,
    item: &BubbleItem,
    pos: egui::Pos2,
    actions: &mut Vec<BubbleAction>,
) {
    let idx = item.session_idx;
    // ── セッションごとに安定した Id（index ではなく安定 ID をキーにする）
    let card_id = egui::Id::new(("zv-pet-bubble", item.key));

    // ── 入場アニメーション: 初回フレームで 0 を植え付けてから 1 へ補間し、
    //    (1 - t) * ANIM_SLIDE だけ下から滑り込ませる（ワンショット）
    let anim_id = card_id.with("anim");
    let seen_id = card_id.with("seen");
    let first_frame = !ctx.data(|d| d.get_temp::<bool>(seen_id).unwrap_or(false));
    if first_frame {
        ctx.data_mut(|d| d.insert_temp(seen_id, true));
        ctx.animate_value_with_time(anim_id, 0.0, 0.0);
    }
    let t = ctx.animate_value_with_time(anim_id, 1.0, ANIM_TIME);
    let pos = egui::pos2(pos.x, pos.y + (1.0 - t) * ANIM_SLIDE);

    egui::Area::new(card_id)
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .interactable(true)
        .show(ctx, |ui| {
            let frame = egui::Frame::none()
                .fill(theme.panel)
                .stroke(egui::Stroke::new(1.0_f32, theme.border))
                .rounding(egui::Rounding::same(8.0))
                .inner_margin(egui::Margin {
                    left: STRIPE_W + 8.0,
                    right: 10.0,
                    top: 8.0,
                    bottom: 8.0,
                });
            let inner = frame.show(ui, |ui| {
                ui.set_width(CARD_W - STRIPE_W - 18.0);
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);

                // ── 1 行目: アイコン + タイトル（省略表示）
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&item.icon).size(15.0));
                    ui.label(
                        egui::RichText::new(truncate_title(&item.title))
                            .color(theme.text)
                            .strong(),
                    );
                });

                // ── 2 行目: ボタン行 [✔ 承認] [✖ 拒否] [開く] [×]
                ui.horizontal(|ui| {
                    if ui
                        .button(egui::RichText::new(tr("✔ 承認")).color(theme.ok))
                        .clicked()
                    {
                        actions.push(BubbleAction::Approve(idx));
                    }
                    if ui
                        .button(egui::RichText::new(tr("✖ 拒否")).color(theme.err))
                        .clicked()
                    {
                        actions.push(BubbleAction::Deny(idx));
                    }
                    if ui
                        .button(egui::RichText::new(tr("開く")).color(theme.accent))
                        .clicked()
                    {
                        actions.push(BubbleAction::Focus(idx));
                    }
                    if ui
                        .small_button(egui::RichText::new("×").color(theme.text_dim))
                        .clicked()
                    {
                        actions.push(BubbleAction::Dismiss(idx));
                    }
                });
            });

            // ── 左端の警告色ストライプ（枠の上に重ね描き、左側のみ角丸）
            let r = inner.response.rect;
            let stripe = egui::Rect::from_min_max(r.min, egui::pos2(r.min.x + STRIPE_W, r.max.y));
            ui.painter().rect_filled(
                stripe,
                egui::Rounding {
                    nw: 8.0,
                    ne: 0.0,
                    sw: 8.0,
                    se: 0.0,
                },
                theme.warn,
            );
        });
}

/// カードが画面内（余白 MARGIN 付き）に完全に収まるよう左上座標をクランプする
fn clamp_to_screen(pos: egui::Pos2, screen: egui::Rect, w: f32, h: f32) -> egui::Pos2 {
    egui::pos2(
        pos.x
            .clamp(screen.left() + MARGIN, (screen.right() - MARGIN - w).max(screen.left() + MARGIN)),
        pos.y
            .clamp(screen.top() + MARGIN, (screen.bottom() - MARGIN - h).max(screen.top() + MARGIN)),
    )
}

/// タイトルを TITLE_MAX_CHARS 文字で切り詰め、超過分を … に置き換える
fn truncate_title(s: &str) -> String {
    if s.chars().count() <= TITLE_MAX_CHARS {
        s.to_string()
    } else {
        let head: String = s.chars().take(TITLE_MAX_CHARS).collect();
        format!("{head}…")
    }
}
