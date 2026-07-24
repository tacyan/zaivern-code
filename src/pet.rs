//! デスクトップペット「ザイガニ」— clawd-on-desk インスパイア。
//! 既定では画面右下をうろうろし、ドラッグで好きな位置へ移動できる。
//! エージェントの状態(稼働中/承認待ち/成功/失敗)にリアクションし、
//! 放置すると居眠り→熟睡、クリック連打で怒り、ダブルクリックで喜ぶ。
//! 見た目はブロック調(サーモン色)のほか、Crab/Cat/Cloud(pet_variants)と
//! ユーザー画像に差し替え可能。

use eframe::egui::{self, Align2, Color32, Pos2, Rect, TextureHandle, Vec2};

use crate::theme::Theme;

// ── 状態(優先度 高→低: Error > Attention > Happy > Annoyed > Groove > Working > Dozing/Sleeping > Roam > Idle)──

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PetState {
    /// 熟睡(横棒の目+ゆっくり呼吸)
    Sleeping,
    /// 居眠り(とろんとした半目)
    Dozing,
    /// 待機(視線がカーソルを追う)
    Idle,
    /// 右下うろうろ(歩いては数秒休む)
    Roam,
    /// 稼働中(n = 稼働エージェント数。足踏み速度が n に比例)
    Working(usize),
    /// ノリノリ(3体以上稼働。大きくバウンス)
    Groove,
    /// 承認待ち(左右にそわそわ)
    Attention,
    /// 成功直後 / ダブルクリック(ジャンプ+にっこり目)
    Happy,
    /// 失敗直後(赤味がかったボディ+バツ目)
    Error,
    /// クリック連打された(ぷるぷる+吊り目)
    Annoyed,
}

// ── 見た目バリアント ──

#[derive(Clone, Copy, PartialEq)]
pub enum PetVariant {
    Blocky,
    Crab,
    Cat,
    Cloud,
}

impl PetVariant {
    /// 設定文字列から復元(不明は Blocky)。
    pub fn from_name(s: &str) -> Self {
        match s {
            "crab" => PetVariant::Crab,
            "cat" => PetVariant::Cat,
            "cloud" => PetVariant::Cloud,
            _ => PetVariant::Blocky,
        }
    }

    /// 設定へ保存する文字列(from_name の逆変換)。
    pub fn name(&self) -> &'static str {
        match self {
            PetVariant::Blocky => "blocky",
            PetVariant::Crab => "crab",
            PetVariant::Cat => "cat",
            PetVariant::Cloud => "cloud",
        }
    }
}

/// 各バリアント描画関数へ渡すフレーム毎のアニメパラメータ。
pub struct DrawParams {
    /// 上下の弾み(px, 上が負)
    pub bob: f32,
    /// 耳・付属パーツの振れ幅(px)
    pub wave: f32,
    /// 足の振り(px)
    pub leg_t: f32,
    /// 視線オフセット(カーソル追従。±1.5*scale px)
    pub eye_look: Vec2,
    /// まばたき中か
    pub blink: bool,
    /// ドラッグ中か(見開き等の演出用)
    pub dragging: bool,
    /// 全体スケール
    pub scale: f32,
    /// 左向きか(ロームの進行方向から決定)
    pub flip_x: bool,
}

/// アプリ側から毎フレーム渡す入力。
pub struct PetInput {
    /// 稼働中エージェント数
    pub working: usize,
    /// 承認待ち数
    pub attention: usize,
    /// 直近で成功があった(Happy 演出)
    pub recent_success: bool,
    /// 直近で失敗があった(Error 演出)
    pub recent_error: bool,
    /// 見た目バリアント
    pub variant: PetVariant,
    /// 表示スケール(1.0 = 66x62px)
    pub scale: f32,
    /// アンカーモード時にうろうろ歩くか
    pub free_roam: bool,
    /// 放置時に居眠り/熟睡するか
    pub sleep_enabled: bool,
}

/// 内部アニメ状態(フレームを跨いで保持)。Default 必須。
#[derive(Default)]
pub struct PetRuntime {
    /// 連打計測用のクリック時刻(直近 ~1.2 秒分)
    click_times: Vec<f64>,
    /// ダブルクリック判定用の直前クリック時刻
    last_click_time: Option<f64>,
    /// 最後にポインタ入力があった時刻(睡眠判定)
    last_input_time: f64,
    /// この時刻まで Annoyed
    annoyed_until: f64,
    /// この時刻まで Happy(ダブルクリックのご機嫌ホップ)
    happy_until: f64,
    /// この時刻まで起き抜けのびっくりホップ
    wake_until: f64,
    /// 前フレームが Dozing/Sleeping だったか(起床検知)
    was_drowsy: bool,
    /// ローム中: 歩行中か(false = 休憩中)
    roam_walking: bool,
    /// ローム: 歩行/休憩の切替時刻
    roam_state_until: f64,
    /// ローム: 歩行中のみ進む位相(sin で往復)
    roam_phase: f64,
    /// 進行方向(true = 左向き)
    flip_x: bool,
    /// dt 計算用の前フレーム時刻
    last_t: f64,
}

#[derive(Default)]
pub struct PetResponse {
    pub clicked: bool,
    pub dragged: bool,
    /// ドラッグが終わったフレーム(位置の保存契機)
    pub drag_released: bool,
    /// ダブルクリックでご機嫌になったフレーム(効果音などの契機)
    pub double_clicked: bool,
    /// ペット矩形の上端中央(スクリーン座標)。吹き出し等のアンカー
    pub bubble_anchor: Option<Pos2>,
}

const BOX_W: f32 = 66.0;
const BOX_H: f32 = 62.0;

// ── 睡眠/リアクションの時間定数(秒)──
const DOZE_AFTER: f64 = 20.0;
const SLEEP_AFTER: f64 = 60.0;
const DOUBLE_CLICK_WINDOW: f64 = 0.35;
const ANNOY_WINDOW: f64 = 1.2;
const ANNOY_CLICKS: usize = 4;
const ANNOY_DURATION: f64 = 2.0;
const HAPPY_HOP_DURATION: f64 = 1.4;
const WAKE_HOP_DURATION: f64 = 0.7;

/// ペットを描画する。
/// `pos`: None なら右下アンカー(free_roam でうろうろ)、Some なら固定位置(ドラッグで更新)。
/// `tex`: Some ならユーザー画像、None なら variant のビルトイン描画。
pub fn draw(
    ctx: &egui::Context,
    theme: &Theme,
    input: &PetInput,
    pos: &mut Option<Pos2>,
    tex: Option<&TextureHandle>,
    rt: &mut PetRuntime,
) -> PetResponse {
    let scale = input.scale.clamp(0.25, 4.0);

    // ── ポインタ入力の観測(睡眠判定と視線追従)──
    let (t, ptr_pos, ptr_active) = ctx.input(|i| {
        let active =
            i.pointer.delta().length() > 0.1 || i.pointer.any_down() || i.pointer.any_pressed();
        (i.time, i.pointer.latest_pos(), active)
    });
    if rt.last_t == 0.0 {
        rt.last_t = t;
        // 初回描画時は入力時刻も初期化する(起動から時間が経った後に
        // 表示をONにしても、いきなり熟睡状態で現れないように)
        rt.last_input_time = t;
    }
    let dt = (t - rt.last_t).clamp(0.0, 0.1);
    rt.last_t = t;

    if ptr_active {
        // 眠っていたら即起床+びっくりホップ
        if rt.was_drowsy {
            rt.wake_until = t + WAKE_HOP_DURATION;
        }
        rt.last_input_time = t;
    }
    let idle_for = t - rt.last_input_time;

    // ── 状態解決(優先度順)──
    let state = resolve_state(input, rt, t, idle_for, pos.is_none());
    rt.was_drowsy = matches!(state, PetState::Dozing | PetState::Sleeping);

    // ── ローム更新(歩いては休むサイクル。位相は歩行中のみ進む)──
    let mut roam_moving = false;
    if state == PetState::Roam {
        if t >= rt.roam_state_until {
            rt.roam_walking = !rt.roam_walking;
            let r = prand(t);
            rt.roam_state_until = t + if rt.roam_walking { 3.0 + r * 3.5 } else { 1.5 + r * 2.5 };
        }
        if rt.roam_walking {
            rt.roam_phase += dt * 0.45;
            // x_off = -(24 + (sin+1)/2 * 130) なので cos>=0 で左(画面内側)へ移動
            rt.flip_x = rt.roam_phase.cos() >= 0.0;
            roam_moving = true;
        }
    }

    // ── 状態ごとのアニメパラメータ ──
    let (bob, wave, leg_t): (f64, f64, f64) = match state {
        PetState::Sleeping => ((t * 1.2).sin() * 0.6, 0.0, 0.0),
        PetState::Dozing => ((t * 1.6).sin() * 1.2, (t * 1.0).sin() * 0.6, 0.0),
        PetState::Idle => {
            // ときどき耳をぴょこぴょこ動かす
            let wiggle = if (t * 0.11).fract() < 0.22 { 2.0 } else { 0.5 };
            ((t * 2.0).sin() * 2.5, (t * 1.6).sin() * wiggle, (t * 1.6).sin() * 0.5)
        }
        PetState::Roam => {
            if roam_moving {
                ((t * 3.4).sin() * 2.0, (t * 3.0).sin() * 1.5, (t * 6.0).sin() * 2.4)
            } else {
                ((t * 2.0).sin() * 1.8, (t * 1.4).sin() * 0.8, 0.0)
            }
        }
        PetState::Working(n) => {
            // 稼働数に応じて足踏みが速くなる
            let sp = 3.0 + (n.min(8) as f64) * 0.7;
            ((t * sp).sin() * 2.2, (t * sp).sin() * 2.0, (t * sp * 1.3).sin() * 2.6)
        }
        PetState::Groove => {
            (-(t * 7.0).sin().abs() * 5.0, (t * 11.0).sin() * 3.2, (t * 9.0).sin() * 2.4)
        }
        PetState::Attention => ((t * 6.4).sin() * 1.6, (t * 6.0).sin() * 2.0, (t * 8.0).sin() * 2.0),
        PetState::Happy => {
            (-(t * 7.0).sin().abs() * 6.0, (t * 9.0).sin() * 2.5, (t * 9.0).sin() * 2.0)
        }
        PetState::Error => ((t * 20.0).sin() * 0.8, 0.5, 0.5),
        PetState::Annoyed => ((t * 4.0).sin() * 1.0, (t * 14.0).sin() * 2.5, (t * 16.0).sin() * 2.0),
    };
    let mut bob = bob as f32 * scale;
    let wave = wave as f32 * scale;
    let leg_t = leg_t as f32 * scale;
    // 起き抜けのびっくりホップ
    if t < rt.wake_until {
        bob -= ((t * 16.0).sin().abs() as f32) * 4.0 * scale;
    }
    let blink = (t * 0.47).fract() < 0.05;
    let flip_x = rt.flip_x;

    // ── 配置: Some = 固定位置 / None = 右下アンカー(free_roam で位相うろうろ)──
    let box_size = egui::vec2(BOX_W * scale, BOX_H * scale);
    let id = egui::Id::new("zv-pet");
    let area = match *pos {
        Some(p) => egui::Area::new(id).order(egui::Order::Foreground).current_pos(p),
        None => {
            let x_off = if input.free_roam {
                -(24.0 + ((rt.roam_phase.sin() as f32) * 0.5 + 0.5) * 130.0)
            } else {
                // free_roam OFF: 定位置でそっと弾むだけ
                -90.0
            };
            egui::Area::new(id)
                .order(egui::Order::Foreground)
                .anchor(Align2::RIGHT_BOTTOM, egui::vec2(x_off, -30.0))
        }
    };

    let inner = area
        .show(ctx, |ui| {
            let (rect, resp) =
                ui.allocate_exact_size(box_size, egui::Sense::click_and_drag());

            // ── 視線: カーソル方向へ ±1.5*scale px(ローム歩行中は進行方向)──
            let mut eye_look = Vec2::ZERO;
            if let Some(pp) = ptr_pos {
                let d = pp - rect.center();
                let m = 1.5 * scale;
                eye_look = egui::vec2(d.x.clamp(-m, m), d.y.clamp(-m, m));
            }
            if roam_moving {
                eye_look.x = if flip_x { -1.5 * scale } else { 1.5 * scale };
            }

            let params = DrawParams {
                bob,
                wave,
                leg_t,
                eye_look,
                blink,
                dragging: resp.dragged(),
                scale,
                flip_x,
            };

            let painter = ui.painter();
            match tex {
                Some(tex) => draw_image(painter, rect, tex, &params),
                None => match input.variant {
                    PetVariant::Blocky => draw_blocky(painter, rect, t, state, &params),
                    PetVariant::Crab => {
                        crate::pet_variants::draw_crab(painter, rect, t, state, &params)
                    }
                    PetVariant::Cat => {
                        crate::pet_variants::draw_cat(painter, rect, t, state, &params)
                    }
                    PetVariant::Cloud => {
                        crate::pet_variants::draw_cloud(painter, rect, t, state, &params)
                    }
                },
            }
            draw_bubble(painter, rect, theme, state);

            // ドラッグ移動: None のときは現在の実位置を確定してから動かす
            if resp.dragged() {
                let base = pos.unwrap_or(rect.min);
                *pos = Some(base + resp.drag_delta());
            }
            let anchor = egui::pos2(rect.center().x, rect.min.y);
            (resp, anchor)
        })
        .inner;

    let (resp, anchor) = inner;
    let clicked = resp.clicked();
    let dragged = resp.dragged();
    let drag_released = resp.drag_stopped();

    // ── クリック解析: ダブルクリック(350ms)でご機嫌 / 1.2 秒に 4 連打で Annoyed ──
    let mut double_clicked = false;
    if clicked {
        match rt.last_click_time {
            Some(last) if t - last < DOUBLE_CLICK_WINDOW => {
                // 怒り中はご機嫌にしない(Happy の優先度が高く怒り顔が隠れてしまうため)
                if t >= rt.annoyed_until {
                    rt.happy_until = t + HAPPY_HOP_DURATION;
                    double_clicked = true;
                }
                rt.last_click_time = None;
            }
            _ => rt.last_click_time = Some(t),
        }
        rt.click_times.push(t);
        rt.click_times.retain(|&c| t - c <= ANNOY_WINDOW);
        if rt.click_times.len() >= ANNOY_CLICKS {
            rt.annoyed_until = t + ANNOY_DURATION;
            // 連打中のダブルクリック判定で付いた Happy を打ち消して怒り顔を見せる
            rt.happy_until = 0.0;
            rt.click_times.clear();
        }
    }

    resp.on_hover_text(
        "ザイガニ 🐾 — クリック: Cockpit/承認待ちへ / ダブルクリック: ご機嫌 / ドラッグ: 移動\n(🐾 メニューで表示・見た目・画像変更)",
    );

    // 熟睡中は再描画間隔を緩めて省電力
    let repaint_ms = if state == PetState::Sleeping { 200 } else { 60 };
    ctx.request_repaint_after(std::time::Duration::from_millis(repaint_ms));

    PetResponse {
        clicked,
        dragged,
        drag_released,
        double_clicked,
        bubble_anchor: Some(anchor),
    }
}

/// 入力とランタイムから現在の状態を優先度順に解決する。
fn resolve_state(
    input: &PetInput,
    rt: &PetRuntime,
    t: f64,
    idle_for: f64,
    anchored: bool,
) -> PetState {
    if input.recent_error {
        return PetState::Error;
    }
    if input.attention > 0 {
        return PetState::Attention;
    }
    if input.recent_success || t < rt.happy_until {
        return PetState::Happy;
    }
    if t < rt.annoyed_until {
        return PetState::Annoyed;
    }
    if input.working >= 3 {
        return PetState::Groove;
    }
    if input.working > 0 {
        return PetState::Working(input.working);
    }
    // ここまで来たら working == 0 && attention == 0
    if input.sleep_enabled {
        if idle_for >= SLEEP_AFTER {
            return PetState::Sleeping;
        }
        if idle_for >= DOZE_AFTER {
            return PetState::Dozing;
        }
    }
    if anchored && input.free_roam {
        return PetState::Roam;
    }
    PetState::Idle
}

/// 決定的な疑似乱数(0..1)。ローム休憩時間のゆらぎ用。
fn prand(seed: f64) -> f64 {
    ((seed * 12.9898).sin() * 43758.5453).fract().abs()
}

/// ユーザー画像モード(スケールと bob を反映)。
fn draw_image(painter: &egui::Painter, rect: Rect, tex: &TextureHandle, p: &DrawParams) {
    let sz = tex.size_vec2();
    let fit = (rect.width() / sz.x).min(rect.height() / sz.y);
    let draw = sz * fit;
    let center = rect.center() + egui::vec2(0.0, p.bob);
    let img_rect = Rect::from_center_size(center, draw);
    // 接地シャドウ
    shadow(painter, rect, p.scale);
    let tint = if p.dragging {
        Color32::from_white_alpha(220)
    } else {
        Color32::WHITE
    };
    painter.image(
        tex.id(),
        img_rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        tint,
    );
}

fn shadow(painter: &egui::Painter, rect: Rect, s: f32) {
    let c = egui::pos2(rect.center().x, rect.max.y - 4.0 * s);
    let sh = Rect::from_center_size(c, egui::vec2(40.0 * s, 9.0 * s));
    painter.rect_filled(sh, egui::Rounding::same(5.0), Color32::from_black_alpha(55));
}

/// ブロック調のペットを描画する(サーモン色の四角ボディ+縦長の目+左右の耳+足4本+地面バー)。
/// 状態ごとに 目の形 / 揺れ / ボディ色 が変わる。
fn draw_blocky(painter: &egui::Painter, rect: Rect, t: f64, state: PetState, p: &DrawParams) {
    let s = p.scale;
    let body_col = match state {
        // 失敗直後は赤味がかったボディ
        PetState::Error => Color32::from_rgb(0xE2, 0x63, 0x4C),
        _ => Color32::from_rgb(0xCF, 0x89, 0x71),
    };
    let eye_col = Color32::from_rgb(0x00, 0x00, 0x00);
    let ground_col = Color32::from_rgb(0x7E, 0x7E, 0x7E);

    // ── 状態ごとの横揺れ(そわそわ / ぷるぷる)──
    let shake = match state {
        PetState::Attention => ((t * 14.0).sin() as f32) * 2.0 * s,
        PetState::Annoyed => ((t * 26.0).sin() as f32) * 2.6 * s,
        PetState::Error => ((t * 30.0).sin() as f32) * 1.0 * s,
        _ => 0.0,
    };
    let gcx = rect.center().x; // 地面バーは揺らさない
    let cx = gcx + shake;

    // ── 寸法(参照画像の比率をボックスに合わせてスケール)──
    let body_w = 46.0 * s;
    let mut body_h = 28.0 * s;
    let leg_h = 9.0 * s;
    let ground_h = 5.0 * s;

    // 熟睡中はゆっくり呼吸(ボディ高さがふくらむ/しぼむ)
    if state == PetState::Sleeping {
        body_h *= 1.0 + ((t * 1.3).sin() as f32) * 0.05;
    }

    let ground_top = rect.max.y - ground_h;
    let body_bottom = ground_top - leg_h + 1.0 * s + p.bob;
    let body_top = body_bottom - body_h;

    // ── 足(4本、左右交互にパタパタ。地面バーの下に潜る)──
    for (i, dx) in [-17.0_f32, -8.0, 8.0, 17.0].into_iter().enumerate() {
        let lift = if i % 2 == 0 {
            p.leg_t.max(0.0)
        } else {
            (-p.leg_t).max(0.0)
        };
        let leg = Rect::from_min_size(
            egui::pos2(cx + dx * s - 2.2 * s, body_bottom - 1.0 * s),
            egui::vec2(4.4 * s, leg_h + 1.0 * s - lift),
        );
        painter.rect_filled(leg, egui::Rounding::ZERO, body_col);
    }

    // ── 地面バー(体が跳ねても動かない)──
    painter.rect_filled(
        Rect::from_min_size(
            egui::pos2(gcx - 20.0 * s, ground_top),
            egui::vec2(40.0 * s, ground_h),
        ),
        egui::Rounding::ZERO,
        ground_col,
    );

    // ── 耳(左右の突起。上下に振る。Groove では高速)──
    let ear_cy = body_top + body_h * 0.55;
    for (sign, dy) in [(-1.0_f32, -p.wave), (1.0_f32, p.wave)] {
        let ear = Rect::from_min_size(
            egui::pos2(
                cx + sign * body_w / 2.0 - (if sign < 0.0 { 8.0 * s } else { 0.0 }),
                ear_cy - 4.5 * s + dy,
            ),
            egui::vec2(8.0 * s, 9.0 * s),
        );
        painter.rect_filled(ear, egui::Rounding::same(1.0), body_col);
    }

    // ── ボディ ──
    let body = Rect::from_min_size(
        egui::pos2(cx - body_w / 2.0, body_top),
        egui::vec2(body_w, body_h),
    );
    painter.rect_filled(body, egui::Rounding::same(2.0), body_col);

    // ── 目(状態で形が変わる。基本は縦長の黒バー+カーソル追従)──
    // 眠っている間は視線追従しない
    let look = if matches!(state, PetState::Sleeping | PetState::Dozing) {
        Vec2::ZERO
    } else {
        p.eye_look
    };
    let eye_w = if p.dragging { 6.0 * s } else { 4.0 * s };
    let eye_h = 9.0 * s;
    let eye_top = body_top + 5.0 * s + look.y;
    for sx in [-1.0_f32, 1.0] {
        let ex = cx + sx * 10.0 * s + look.x; // 目の中心X
        let ecy = eye_top + eye_h / 2.0;
        match state {
            PetState::Error => {
                // バツ目(2本の交差ストローク)
                let r = 3.2 * s;
                let st = egui::Stroke::new(2.0 * s, eye_col);
                painter.line_segment([egui::pos2(ex - r, ecy - r), egui::pos2(ex + r, ecy + r)], st);
                painter.line_segment([egui::pos2(ex - r, ecy + r), egui::pos2(ex + r, ecy - r)], st);
            }
            PetState::Happy => {
                // にっこり(上向きアーチの ∧ 目)
                let r = 3.0 * s;
                let st = egui::Stroke::new(2.0 * s, eye_col);
                painter.line_segment(
                    [egui::pos2(ex - r, ecy + 1.5 * s), egui::pos2(ex, ecy - 2.5 * s)],
                    st,
                );
                painter.line_segment(
                    [egui::pos2(ex, ecy - 2.5 * s), egui::pos2(ex + r, ecy + 1.5 * s)],
                    st,
                );
            }
            PetState::Annoyed => {
                // 吊り目(外側が上、内側が下の ＼ ／)
                let r = 2.8 * s;
                let st = egui::Stroke::new(2.2 * s, eye_col);
                painter.line_segment(
                    [
                        egui::pos2(ex + sx * r, ecy - 2.0 * s),
                        egui::pos2(ex - sx * r, ecy + 1.5 * s),
                    ],
                    st,
                );
            }
            PetState::Sleeping => {
                // 閉じた横棒(高さ2px相当)
                let bar = Rect::from_center_size(
                    egui::pos2(ex, eye_top + eye_h - 1.0 * s),
                    egui::vec2(6.0 * s, 2.0 * s),
                );
                painter.rect_filled(bar, egui::Rounding::ZERO, eye_col);
            }
            PetState::Dozing => {
                // とろんとした半目(下半分だけ)
                let half = Rect::from_min_size(
                    egui::pos2(ex - eye_w / 2.0, eye_top + eye_h * 0.45),
                    egui::vec2(eye_w, eye_h * 0.55),
                );
                painter.rect_filled(half, egui::Rounding::ZERO, eye_col);
            }
            _ => {
                if p.blink {
                    let bar = Rect::from_min_size(
                        egui::pos2(ex - eye_w / 2.0, eye_top + eye_h - 2.0 * s),
                        egui::vec2(eye_w, 2.0 * s),
                    );
                    painter.rect_filled(bar, egui::Rounding::ZERO, eye_col);
                } else {
                    let eye = Rect::from_min_size(
                        egui::pos2(ex - eye_w / 2.0, eye_top),
                        egui::vec2(eye_w, eye_h),
                    );
                    painter.rect_filled(eye, egui::Rounding::ZERO, eye_col);
                }
            }
        }
    }
}

/// 状態に応じた吹き出しをペットの頭上に描く。
fn draw_bubble(painter: &egui::Painter, rect: Rect, theme: &Theme, state: PetState) {
    let bubble: Option<(String, Color32)> = match state {
        PetState::Attention => Some(("❗承認待ち".into(), theme.warn)),
        PetState::Working(n) => Some((format!("⚙ {n}"), theme.accent)),
        PetState::Groove => Some(("🎵".into(), theme.accent)),
        PetState::Error => Some(("💥".into(), theme.err)),
        PetState::Happy => Some(("🎉".into(), theme.ok)),
        PetState::Sleeping => Some(("💤".into(), theme.text_dim)),
        _ => None,
    };
    if let Some((txt, color)) = bubble {
        let galley = painter.layout_no_wrap(txt, egui::FontId::proportional(12.0), theme.text);
        let pos = egui::pos2(
            rect.center().x - galley.size().x / 2.0,
            rect.min.y - galley.size().y - 4.0,
        );
        let bg = Rect::from_min_size(pos, galley.size()).expand(4.0);
        painter.rect_filled(bg, 6.0, theme.panel);
        painter.rect_stroke(bg, 6.0, egui::Stroke::new(1.0_f32, color.gamma_multiply(0.8)));
        painter.galley(pos, galley, theme.text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全て「何も起きていない」状態の入力。各テストで必要なフィールドだけ上書きする。
    fn base_input() -> PetInput {
        PetInput {
            working: 0,
            attention: 0,
            recent_success: false,
            recent_error: false,
            variant: PetVariant::Blocky,
            scale: 1.0,
            free_roam: false,
            sleep_enabled: false,
        }
    }

    /// resolve_state のショートハンド(rt はデフォルト、t=100.0)。
    fn resolve(input: &PetInput, idle_for: f64, anchored: bool) -> PetState {
        resolve_state(input, &PetRuntime::default(), 100.0, idle_for, anchored)
    }

    // ── resolve_state: 優先順位 ──

    #[test]
    fn recent_error_wins_over_everything() {
        let mut input = base_input();
        input.recent_error = true;
        input.attention = 5;
        input.recent_success = true;
        input.working = 10;
        input.sleep_enabled = true;
        input.free_roam = true;
        assert_eq!(resolve(&input, 1000.0, true), PetState::Error);
    }

    #[test]
    fn attention_beats_success_and_working() {
        let mut input = base_input();
        input.attention = 1;
        input.recent_success = true;
        input.working = 10;
        assert_eq!(resolve(&input, 0.0, false), PetState::Attention);
    }

    #[test]
    fn happy_from_recent_success_or_happy_until() {
        let mut input = base_input();
        input.recent_success = true;
        input.working = 10;
        assert_eq!(resolve(&input, 0.0, false), PetState::Happy);

        // recent_success が無くても t < happy_until なら Happy(Annoyed より優先)
        let input = base_input();
        let rt = PetRuntime {
            happy_until: 200.0,
            annoyed_until: 200.0,
            ..Default::default()
        };
        assert_eq!(
            resolve_state(&input, &rt, 100.0, 0.0, false),
            PetState::Happy
        );
    }

    #[test]
    fn annoyed_until_beats_working() {
        let mut input = base_input();
        input.working = 5;
        let rt = PetRuntime {
            annoyed_until: 200.0,
            ..Default::default()
        };
        assert_eq!(
            resolve_state(&input, &rt, 100.0, 0.0, false),
            PetState::Annoyed
        );
        // 期限切れ(t >= annoyed_until)なら通常の解決に戻る
        assert_eq!(
            resolve_state(&input, &rt, 200.0, 0.0, false),
            PetState::Groove
        );
    }

    #[test]
    fn working_count_boundaries() {
        let mut input = base_input();
        input.working = 1;
        assert_eq!(resolve(&input, 0.0, false), PetState::Working(1));
        input.working = 2;
        assert_eq!(resolve(&input, 0.0, false), PetState::Working(2));
        // 3 以上で Groove
        input.working = 3;
        assert_eq!(resolve(&input, 0.0, false), PetState::Groove);
        input.working = 100;
        assert_eq!(resolve(&input, 0.0, false), PetState::Groove);
    }

    #[test]
    fn working_beats_sleep() {
        let mut input = base_input();
        input.working = 1;
        input.sleep_enabled = true;
        assert_eq!(resolve(&input, SLEEP_AFTER * 10.0, false), PetState::Working(1));
    }

    #[test]
    fn doze_and_sleep_thresholds() {
        let mut input = base_input();
        input.sleep_enabled = true;
        // DOZE_AFTER 未満は眠らない
        assert_eq!(resolve(&input, DOZE_AFTER - 0.001, false), PetState::Idle);
        // DOZE_AFTER 以上 SLEEP_AFTER 未満は Dozing
        assert_eq!(resolve(&input, DOZE_AFTER, false), PetState::Dozing);
        assert_eq!(resolve(&input, SLEEP_AFTER - 0.001, false), PetState::Dozing);
        // SLEEP_AFTER 以上は Sleeping
        assert_eq!(resolve(&input, SLEEP_AFTER, false), PetState::Sleeping);
    }

    #[test]
    fn sleep_disabled_never_dozes() {
        let mut input = base_input();
        input.sleep_enabled = false;
        assert_eq!(resolve(&input, SLEEP_AFTER * 10.0, false), PetState::Idle);
    }

    #[test]
    fn sleeping_beats_roam() {
        let mut input = base_input();
        input.sleep_enabled = true;
        input.free_roam = true;
        assert_eq!(resolve(&input, SLEEP_AFTER, true), PetState::Sleeping);
    }

    #[test]
    fn roam_requires_anchored_and_free_roam() {
        let mut input = base_input();
        input.free_roam = true;
        assert_eq!(resolve(&input, 0.0, true), PetState::Roam);
        // アンカーモードでなければ Idle
        assert_eq!(resolve(&input, 0.0, false), PetState::Idle);
        // free_roam でなければ Idle
        input.free_roam = false;
        assert_eq!(resolve(&input, 0.0, true), PetState::Idle);
    }

    // ── PetVariant: from_name / name ──

    #[test]
    fn variant_name_roundtrip() {
        // PetVariant は Debug 未導出のため assert! で比較する
        for v in [
            PetVariant::Blocky,
            PetVariant::Crab,
            PetVariant::Cat,
            PetVariant::Cloud,
        ] {
            assert!(
                PetVariant::from_name(v.name()) == v,
                "roundtrip failed for {}",
                v.name()
            );
        }
        assert_eq!(PetVariant::Blocky.name(), "blocky");
        assert_eq!(PetVariant::Crab.name(), "crab");
        assert_eq!(PetVariant::Cat.name(), "cat");
        assert_eq!(PetVariant::Cloud.name(), "cloud");
    }

    #[test]
    fn variant_unknown_name_falls_back_to_blocky() {
        for s in ["", "unknown", "Crab", "CAT", "blocky ", "dog"] {
            assert!(
                PetVariant::from_name(s) == PetVariant::Blocky,
                "expected Blocky for {:?}",
                s
            );
        }
        // 既定文字列 "blocky" 自身も Blocky
        assert!(PetVariant::from_name("blocky") == PetVariant::Blocky);
    }
}
