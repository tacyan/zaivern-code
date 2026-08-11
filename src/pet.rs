//! デスクトップペット「ザイガニ」— clawd-on-desk インスパイア。
//! 既定では画面右下をうろうろし、ドラッグで好きな位置へ移動できる。
//! エージェントの状態(稼働中/承認待ち/成功/失敗)にリアクションし、
//! 放置すると居眠り→熟睡、クリック連打で怒り、ダブルクリックで喜ぶ。
//! 見た目はブロック調(サーモン色)のほか、Crab/Cat/Cloud(pet_variants)と
//! ユーザー画像に差し替え可能。

use eframe::egui::{self, Color32, Pos2, Rect, TextureHandle, Vec2};

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

/// 画面端との最小すき間 (ペットの拡大率に比例)。
///
/// ボックスの外へ数 px はみ出す部位 (耳・そわそわの横揺れ・影) があるので、
/// 枠ぴったりに寄せると絵が切れる。`space::SM` 相当を基準にして倍率を掛ける。
const EDGE_MARGIN: f32 = crate::panels::space::SM;

/// 右下アンカー時の既定オフセット (右へ `-x`、下へ `-y`)。
const ANCHOR_X: f32 = 24.0;
const ANCHOR_Y: f32 = 30.0;

/// **ペットの描画枠を求める純関数。**
///
/// `want` が `Some` ならその左上を希望位置に、`None` なら右下アンカー
/// (`roam_x` はうろうろの位相ぶんの左シフト、0 以上) を希望位置にする。
///
/// 返り値は**必ず** `viewport` を `margin` だけ内側へ狭めた矩形に収まる
/// (ビューポートの方が狭ければ左上へ寄せる)。窓を縮めても、前回ドラッグした
/// 位置を憶えていても、サブディスプレイから本ディスプレイへ移しても、
/// ペットが画面外へ消えないための唯一の関門。
pub fn pet_rect(viewport: Rect, size: Vec2, want: Option<Pos2>, roam_x: f32, margin: f32) -> Rect {
    let desired = want.unwrap_or_else(|| {
        egui::pos2(
            viewport.right() - ANCHOR_X - roam_x.max(0.0) - size.x,
            viewport.bottom() - ANCHOR_Y - size.y,
        )
    });
    // 収まる範囲。ビューポートが箱より狭いときは min > max になるので、
    // 先に max を min 側へ丸めて「左上に寄せる」を選ぶ。
    let min = viewport.min + Vec2::splat(margin);
    let max_x = (viewport.right() - margin - size.x).max(min.x);
    let max_y = (viewport.bottom() - margin - size.y).max(min.y);
    let p = egui::pos2(desired.x.clamp(min.x, max_x), desired.y.clamp(min.y, max_y));
    Rect::from_min_size(p, size)
}

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
    //
    // **ホイールとタッチも「触った」に数える。** 端末を 10 分スクロールして
    // いる間ずっと熟睡している、というのは目の前の事実と食い違う。
    // 逆に `delta` の閾値 (0.1px) は残す — マウスの微振動 1 カウントで
    // [`SLEEP_AFTER`] の秒読みが振り出しに戻るのは「触った」ではない。
    let (t, ptr_pos, ptr_active) = ctx.input(|i| {
        let scrolled = i.events.iter().any(|e| {
            matches!(
                e,
                egui::Event::MouseWheel { .. } | egui::Event::Touch { .. }
            )
        });
        let active = i.pointer.delta().length() > 0.1
            || i.pointer.any_down()
            || i.pointer.any_pressed()
            || scrolled;
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
    // **絵が止まっているか。** ここから下の「時刻に依存する計算」は全部これで殺す。
    let still = is_still(state, idle_for);
    rt.was_drowsy = matches!(state, PetState::Dozing | PetState::Sleeping);

    // ── ローム更新(歩いては休むサイクル。位相は歩行中のみ進む)──
    //
    // `still` のときは**位相も進めない**。`roam_phase` は描画位置
    // (`roam_x`) に効くので、進めた時点で「絵は静止画」が嘘になり、
    // 止めたはずのペットがじりじり横へ流れる。
    let mut roam_moving = false;
    if state == PetState::Roam && !still {
        if t >= rt.roam_state_until {
            rt.roam_walking = !rt.roam_walking;
            let r = prand(t);
            rt.roam_state_until = t + if rt.roam_walking {
                3.0 + r * 3.5
            } else {
                1.5 + r * 2.5
            };
        }
        if rt.roam_walking {
            rt.roam_phase += dt * 0.45;
            // x_off = -(24 + (sin+1)/2 * 130) なので cos>=0 で左(画面内側)へ移動
            rt.flip_x = rt.roam_phase.cos() >= 0.0;
            roam_moving = true;
        }
    }

    // ── 状態ごとのアニメパラメータ ──
    //
    // `still` ([`is_still`]) のときは時刻に依存しない**静止画**にする。
    // ±0.6px の上下は目で追えないのに、そのために毎秒フレームを回し続けて
    // いた (アイドル時の CPU の主因)。止めれば「何も起きていないときは 1 枚も
    // 描かない」が成立し、ポインタが動いた瞬間に起床ホップで動き出す。
    let (bob, wave, leg_t): (f64, f64, f64) = if still {
        // 静止 — いつ描いても同じ絵。これが「再描画を 1 枚も予約しない」の前提。
        (0.0, 0.0, 0.0)
    } else {
        match state {
            PetState::Sleeping => (0.0, 0.0, 0.0),
            PetState::Dozing => ((t * 1.6).sin() * 1.2, (t * 1.0).sin() * 0.6, 0.0),
            PetState::Idle => {
                // ときどき耳をぴょこぴょこ動かす
                let wiggle = if (t * 0.11).fract() < 0.22 { 2.0 } else { 0.5 };
                (
                    (t * 2.0).sin() * 2.5,
                    (t * 1.6).sin() * wiggle,
                    (t * 1.6).sin() * 0.5,
                )
            }
            PetState::Roam => {
                if roam_moving {
                    (
                        (t * 3.4).sin() * 2.0,
                        (t * 3.0).sin() * 1.5,
                        (t * 6.0).sin() * 2.4,
                    )
                } else {
                    ((t * 2.0).sin() * 1.8, (t * 1.4).sin() * 0.8, 0.0)
                }
            }
            PetState::Working(n) => {
                // 稼働数に応じて足踏みが速くなる
                let sp = 3.0 + (n.min(8) as f64) * 0.7;
                (
                    (t * sp).sin() * 2.2,
                    (t * sp).sin() * 2.0,
                    (t * sp * 1.3).sin() * 2.6,
                )
            }
            PetState::Groove => (
                -(t * 7.0).sin().abs() * 5.0,
                (t * 11.0).sin() * 3.2,
                (t * 9.0).sin() * 2.4,
            ),
            PetState::Attention => (
                (t * 6.4).sin() * 1.6,
                (t * 6.0).sin() * 2.0,
                (t * 8.0).sin() * 2.0,
            ),
            PetState::Happy => (
                -(t * 7.0).sin().abs() * 6.0,
                (t * 9.0).sin() * 2.5,
                (t * 9.0).sin() * 2.0,
            ),
            PetState::Error => ((t * 20.0).sin() * 0.8, 0.5, 0.5),
            PetState::Annoyed => (
                (t * 4.0).sin() * 1.0,
                (t * 14.0).sin() * 2.5,
                (t * 16.0).sin() * 2.0,
            ),
        }
    };
    let mut bob = bob as f32 * scale;
    let wave = wave as f32 * scale;
    let leg_t = leg_t as f32 * scale;
    // 起き抜けのびっくりホップ
    if t < rt.wake_until {
        bob -= ((t * 16.0).sin().abs() as f32) * 4.0 * scale;
    }
    // まばたきも時刻依存。静止中に残すと「たまに 1 フレームだけ目が閉じた絵」に
    // なり、そのフレームを誰も予約しないので**閉じたまま固まる**ことがある。
    let blink = !still && (t * 0.47).fract() < 0.05;
    // **バリアント描画へ渡す時計。** 静止中は止める。
    //
    // `pet_variants::state_motion` は `Sleeping` にも呼吸 (`squash`) を入れて
    // いるので、ここで `t` をそのまま渡すと「熟睡は静止画」が**バリアントでは
    // 嘘**になる (Crab/Cat/Cloud で実際にそうだった)。時計そのものを止めれば、
    // 後から足したバリアントも自動的に静止する。
    let anim_t = if still { 0.0 } else { t };
    let flip_x = rt.flip_x;

    // ── 配置: Some = 固定位置 / None = 右下アンカー(free_roam で位相うろうろ)──
    //
    // **`Area::anchor` は使わない。** アンカーは egui 内部で画面矩形から解決
    // されるので、こちら側でクランプが効かず、窓を縮めた瞬間にペットが端から
    // はみ出す。位置は毎フレーム [`pet_rect`] で自前に決めてビューポート内へ
    // 収め、`current_pos` で渡す (ドラッグで憶えた位置も同じ関門を通る)。
    let box_size = egui::vec2(BOX_W * scale, BOX_H * scale);
    let roam_x = if input.free_roam {
        ((rt.roam_phase.sin() as f32) * 0.5 + 0.5) * 130.0 * scale
    } else {
        // free_roam OFF: 定位置でそっと弾むだけ
        66.0 * scale
    };
    let viewport = ctx.screen_rect();
    let placed = pet_rect(viewport, box_size, *pos, roam_x, EDGE_MARGIN * scale);
    // 憶えている位置も画面内へ引き戻す (窓を縮めたまま次回起動しても迷子にしない)。
    if pos.is_some() {
        *pos = Some(placed.min);
    }
    let id = egui::Id::new("zv-pet");
    let area = egui::Area::new(id)
        .order(egui::Order::Foreground)
        .current_pos(placed.min);

    let inner = area
        .show(ctx, |ui| {
            let (rect, resp) = ui.allocate_exact_size(box_size, egui::Sense::click_and_drag());

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
                    PetVariant::Blocky => draw_blocky(painter, rect, anim_t, state, &params),
                    PetVariant::Crab => {
                        crate::pet_variants::draw_crab(painter, rect, anim_t, state, &params)
                    }
                    PetVariant::Cat => {
                        crate::pet_variants::draw_cat(painter, rect, anim_t, state, &params)
                    }
                    PetVariant::Cloud => {
                        crate::pet_variants::draw_cloud(painter, rect, anim_t, state, &params)
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

    // 再描画は「本当に絵が変わるとき」だけ要求する。
    let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
    if let Some(ms) = repaint_ms(state, focused, still) {
        crate::perf::repaint_after(
            ctx,
            std::time::Duration::from_millis(ms),
            repaint_tag(state),
        );
    }

    PetResponse {
        clicked,
        dragged,
        drag_released,
        double_clicked,
        bubble_anchor: Some(anchor),
    }
}

/// **絵が時刻に依存しない (= 描き直しても 1px も変わらない) 状態か。**
///
/// `Sleeping` は元から静止画。それに加えて、**`pet_sleep = false` でも**
/// 放置が [`SLEEP_AFTER`] を超えたら動きを止める。設定で
/// 「アイドル時のコストはゼロ」(設計原則 3) を外せてはいけない —
/// 「眠らない」は*目を閉じない*であって、*永久に描き続ける*ではない。
///
/// ここに載る状態は `working == 0 && attention == 0` かつリアクション中でも
/// ない ([`resolve_state`] の優先順位) ので、**止めて困る進捗表示は無い**。
fn is_still(state: PetState, idle_for: f64) -> bool {
    match state {
        PetState::Sleeping => true,
        PetState::Dozing | PetState::Idle | PetState::Roam => idle_for >= SLEEP_AFTER,
        _ => false,
    }
}

/// 状態ごとの再描画間隔 (ms)。`None` は「絵が変わらないので予約しない」。
///
/// `still` ([`is_still`]) なら常に `None`。ポインタが動けば egui が入力で
/// フレームを起こし、そのフレームで起床ホップに移るため、止まったまま
/// 反応しなくなることはない。
///
/// ## 刻みの決め方 — **ペットがアプリで一番速いスケジューラであってはならない**
///
/// `app::idle_repaint_ms` はアプリ自身の予約表を持っている
/// (エージェント実行中 250ms / 背面 1500ms、家事だけなら 2000ms / 背面 6000ms、
///  何も無ければ**予約しない**)。ペットがそれより短い刻みを出した瞬間、
/// **ペットがフレームレートの決定者になる** = 常時アニメーションになる。
///
/// 実測 (v0.14.0, debug, macOS): 直しの前はどの状態でもペットの方が速く、
/// **ポインタが 1 回動くたびに 60 秒間・600 回**の要求が出ていた
/// (`pet::sleep_tests::一度の入力で出る要求の総量に上限がある` で計数。
///  前面 Idle 80ms × 20 秒 + Dozing 160ms × 40 秒)。
/// この表にしてからは **89 回**。眠るまでの秒数 ([`SLEEP_AFTER`]) は変えて
/// いないので、**見た目の挙動はそのまま**で 6.7 分の 1 になる。
///
/// そこで **アプリの刻みを上限にする**:
/// - `Working` / `Groove` / `Attention` … エージェントが走っている状況なので
///   `app::IDLE_AGENT_MS` (250 / 1500) と**同じ**にする。ペットは
///   アプリが既に頼んだフレームに相乗りするだけになり、駆動源にならない
/// - `Idle` / `Roam` / `Dozing` … 時間の上限が無いので家事 (2000ms) を超えない
/// - `Happy` / `Annoyed` / `Error` / 起き抜け … **必ず数秒で終わる**
///   (Happy 4s / Error 6s / Annoyed 2s) ので、ここだけ滑らかに描いてよい。
///   総量は 6 秒 × 12.5fps = 75 枚で頭打ち
fn repaint_ms(state: PetState, focused: bool, still: bool) -> Option<u64> {
    if still {
        return None;
    }
    match state {
        PetState::Sleeping => None,
        // 放置中。**時間の上限が無い**ので、アプリの家事 (2000ms) より速くしない。
        PetState::Idle | PetState::Roam => Some(if focused { 500 } else { 2000 }),
        // うとうとはゆっくりした上下だけ。Idle よりさらに粗く。
        PetState::Dozing => Some(if focused { 1000 } else { 2000 }),
        // エージェントが走っている状況 = app が 250 / 1500 で回している。
        // **同じ刻みに揃えて、ペットが駆動源にならないようにする。**
        PetState::Working(_) | PetState::Groove | PetState::Attention => {
            Some(if focused { 250 } else { 1500 })
        }
        // 数秒で必ず終わる演出だけ滑らかに描く。
        _ => Some(if focused { 80 } else { 250 }),
    }
}

/// 再描画要求の出所タグ。**状態まで割る。**
///
/// `pet_anim` の 1 本だけだと「ペットが描かせている」までしか分からず、
/// **どの状態で自走しているか**が読めない。実際に `perf::dump()` の
/// `pet_anim x251` から状態を逆算するのに、刻みの表と突き合わせる必要があった。
/// 状態ごとに割っておけば、レポートを 1 行見るだけで犯人が確定する。
fn repaint_tag(state: PetState) -> &'static str {
    match state {
        PetState::Sleeping => "pet_anim.sleep",
        PetState::Dozing => "pet_anim.doze",
        PetState::Idle | PetState::Roam => "pet_anim.idle",
        PetState::Working(_) | PetState::Groove => "pet_anim.work",
        PetState::Attention => "pet_anim.attention",
        PetState::Happy | PetState::Error | PetState::Annoyed => "pet_anim.react",
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

    // 熟睡中はほんの少しふくらんだ姿で固定する。
    // ここを時刻依存にすると「熟睡中は再描画しない」(下の repaint 判定) と
    // 噛み合わず、別の理由でフレームが来た瞬間だけ呼吸が飛ぶ。
    // 静止させれば、いつ描いても同じ絵になる。
    if state == PetState::Sleeping {
        body_h *= 1.03;
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
                painter.line_segment(
                    [egui::pos2(ex - r, ecy - r), egui::pos2(ex + r, ecy + r)],
                    st,
                );
                painter.line_segment(
                    [egui::pos2(ex - r, ecy + r), egui::pos2(ex + r, ecy - r)],
                    st,
                );
            }
            PetState::Happy => {
                // にっこり(上向きアーチの ∧ 目)
                let r = 3.0 * s;
                let st = egui::Stroke::new(2.0 * s, eye_col);
                painter.line_segment(
                    [
                        egui::pos2(ex - r, ecy + 1.5 * s),
                        egui::pos2(ex, ecy - 2.5 * s),
                    ],
                    st,
                );
                painter.line_segment(
                    [
                        egui::pos2(ex, ecy - 2.5 * s),
                        egui::pos2(ex + r, ecy + 1.5 * s),
                    ],
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
        painter.rect_stroke(
            bg,
            6.0,
            egui::Stroke::new(1.0_f32, color.gamma_multiply(0.8)),
        );
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
        assert_eq!(
            resolve(&input, SLEEP_AFTER * 10.0, false),
            PetState::Working(1)
        );
    }

    #[test]
    fn doze_and_sleep_thresholds() {
        let mut input = base_input();
        input.sleep_enabled = true;
        // DOZE_AFTER 未満は眠らない
        assert_eq!(resolve(&input, DOZE_AFTER - 0.001, false), PetState::Idle);
        // DOZE_AFTER 以上 SLEEP_AFTER 未満は Dozing
        assert_eq!(resolve(&input, DOZE_AFTER, false), PetState::Dozing);
        assert_eq!(
            resolve(&input, SLEEP_AFTER - 0.001, false),
            PetState::Dozing
        );
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

    // ── 再描画ポリシー ────────────────────────────────────────────

    /// 動いている状態(まだ静止していない)の全部。
    const MOVING: [PetState; 9] = [
        PetState::Idle,
        PetState::Dozing,
        PetState::Roam,
        PetState::Working(1),
        PetState::Groove,
        PetState::Happy,
        PetState::Annoyed,
        PetState::Attention,
        PetState::Error,
    ];

    /// 回帰テスト: 熟睡中は 1 枚も描き直さない。
    /// ここが Some に戻るとアイドル時に常時フレームが回る。
    #[test]
    fn sleeping_never_asks_for_a_frame() {
        for focused in [true, false] {
            for still in [true, false] {
                assert_eq!(repaint_ms(PetState::Sleeping, focused, still), None);
            }
        }
    }

    /// **静止したら、どの状態でも・前面でも 1 枚も予約しない。**
    ///
    /// `pet_sleep = false` でも設計原則 3 を外せないことの本体。
    #[test]
    fn 静止したら前面でも一枚も予約しない() {
        for st in MOVING {
            for focused in [true, false] {
                assert_eq!(
                    repaint_ms(st, focused, true),
                    None,
                    "{st:?} focused={focused}: 静止中に予約している"
                );
            }
        }
    }

    /// 動いている状態は必ず予約する (止まって見えたらバグ)。
    #[test]
    fn moving_states_keep_animating() {
        for st in MOVING {
            assert!(
                repaint_ms(st, true, false).is_some(),
                "{st:?} は前景で動き続けるはず"
            );
        }
    }

    /// 背面では刻みを粗くする。ただし止めはしない (進捗が伝わらなくなる)。
    #[test]
    fn unfocused_backs_off_but_keeps_moving() {
        for st in [PetState::Working(2), PetState::Attention, PetState::Idle] {
            let fg = repaint_ms(st, true, false).expect("前景では予約する");
            let bg = repaint_ms(st, false, false).expect("背面でも止めない");
            assert!(bg > fg, "{st:?}: 背面 {bg}ms は前景 {fg}ms より粗いはず");
        }
    }

    /// うとうと (Dozing) は待機より粗くてよい (ゆっくりした上下だけ)。
    #[test]
    fn dozing_is_cheaper_than_full_animation() {
        let doze = repaint_ms(PetState::Dozing, true, false).expect("Dozing は動く");
        let idle = repaint_ms(PetState::Idle, true, false).expect("Idle は動く");
        assert!(doze > idle, "Dozing {doze}ms は Idle {idle}ms より粗いはず");
    }

    /// **ペットはアプリで一番速いスケジューラであってはならない。**
    ///
    /// `app::idle_repaint_ms` の刻みより短い要求を出した瞬間、ペットが
    /// フレームレートの決定者になる = 常時アニメーション。**時間の上限が
    /// 無い状態**だけを見る (数秒で終わる演出は総量が頭打ちになるので別枠)。
    #[test]
    fn 終わりの無い状態はアプリの刻みを超えない() {
        // app.rs の定数 (エージェント実行中 / 家事)。ここを下回ってはいけない。
        const APP_AGENT_MS: u64 = 250;
        const APP_AGENT_BG_MS: u64 = 1500;
        const APP_HOUSEKEEP_MS: u64 = 2000;

        for st in [PetState::Working(1), PetState::Groove, PetState::Attention] {
            let fg = repaint_ms(st, true, false).expect("動く");
            let bg = repaint_ms(st, false, false).expect("動く");
            assert!(
                fg >= APP_AGENT_MS,
                "{st:?}: 前面 {fg}ms < app {APP_AGENT_MS}ms"
            );
            assert!(
                bg >= APP_AGENT_BG_MS,
                "{st:?}: 背面 {bg}ms < app {APP_AGENT_BG_MS}ms"
            );
        }
        for st in [PetState::Idle, PetState::Roam, PetState::Dozing] {
            let bg = repaint_ms(st, false, false).expect("動く");
            assert!(
                bg >= APP_HOUSEKEEP_MS,
                "{st:?}: 背面 {bg}ms < app の家事 {APP_HOUSEKEEP_MS}ms"
            );
        }
    }

    /// **放置が続く状態が、数秒で終わる演出より安いこと。**
    ///
    /// `Idle` / `Dozing` / `Working` は何分でも続きうる。ここを
    /// 「短命な演出」と同じ刻みにすると、その間ずっとフレームを回し続ける
    /// (実測で「アイドル時の再描画要求の 100% がペット」だった原因がこれ)。
    #[test]
    fn 終わりの無い状態は短命な演出より安い() {
        // 数秒で必ず終わる: Happy 4s / Error 6s / Annoyed 2s (app.rs)
        let short = [PetState::Happy, PetState::Error, PetState::Annoyed];
        let long = [
            PetState::Idle,
            PetState::Roam,
            PetState::Dozing,
            PetState::Working(1),
            PetState::Groove,
            PetState::Attention,
        ];
        for focused in [true, false] {
            for l in long {
                let lm = repaint_ms(l, focused, false).expect("動く");
                for sh in short {
                    let sm = repaint_ms(sh, focused, false).expect("動く");
                    assert!(
                        lm > sm,
                        "focused={focused}: {l:?} {lm}ms は {sh:?} {sm}ms より粗いはず"
                    );
                }
            }
        }
    }

    /// **どの状態も 12.5fps を超えて回さない。**
    /// これが破れると「常時アニメーションはバッテリーのバグ」に逆戻りする。
    #[test]
    fn 前景でも_12fps_を超えて回さない() {
        for st in MOVING {
            if let Some(ms) = repaint_ms(st, true, false) {
                assert!(ms >= 80, "{st:?}: {ms}ms は速すぎる (>12.5fps)");
            }
        }
    }

    /// 出所タグは状態ごとに割れていて、必ず `pet_anim.` で始まる。
    /// (`perf::dump()` の 1 行から犯人が確定できることの担保)
    #[test]
    fn 出所タグは状態ごとに割れている() {
        use std::collections::BTreeSet;
        let mut seen = BTreeSet::new();
        for st in MOVING.into_iter().chain([PetState::Sleeping]) {
            let tag = repaint_tag(st);
            assert!(tag.starts_with("pet_anim."), "{st:?}: {tag}");
            seen.insert(tag);
        }
        // 少なくとも「放置 / うとうと / 作業 / 承認待ち / 演出 / 熟睡」は分かれる
        assert!(seen.len() >= 6, "タグが割れていない: {seen:?}");
    }

    // ── 静止の判定 (is_still) ──────────────────────────────────────

    /// `SLEEP_AFTER` を超えた放置は、**睡眠設定に関係なく**止まる。
    #[test]
    fn 放置がsleep_afterを超えたら睡眠設定に関係なく止まる() {
        for st in [PetState::Idle, PetState::Roam, PetState::Dozing] {
            assert!(!is_still(st, SLEEP_AFTER - 0.001), "{st:?} は早すぎる");
            assert!(is_still(st, SLEEP_AFTER), "{st:?} が止まらない");
            assert!(is_still(st, SLEEP_AFTER * 10.0), "{st:?} が止まらない");
        }
        // 熟睡は放置時間に依らず静止画
        assert!(is_still(PetState::Sleeping, 0.0));
    }

    /// **進捗を伝える状態は、どれだけ放置しても止めない。**
    /// (エージェントが走っているのに固まって見えるのは別のバグになる)
    #[test]
    fn 進捗を伝える状態は放置しても止めない() {
        for st in [
            PetState::Working(1),
            PetState::Groove,
            PetState::Attention,
            PetState::Happy,
            PetState::Error,
            PetState::Annoyed,
        ] {
            assert!(
                !is_still(st, SLEEP_AFTER * 100.0),
                "{st:?} を止めてしまった"
            );
        }
    }
}

/// ペットの配置 (画面外へ出さない)。
///
/// 実際に起きていた不具合: 右下アンカーのまま窓を狭めると、ペットが右端で
/// 半分切れる。ドラッグして憶えた位置は、次に狭い窓で開くと完全に画面外へ出る。
#[cfg(test)]
mod placement_tests {
    use super::*;

    /// 代表的なビューポート (狭い / 普通 / 広い / 箱より小さい)。
    fn viewports() -> Vec<Rect> {
        [
            (900.0_f32, 700.0_f32),
            (1400.0, 900.0),
            (1720.0, 1148.0),
            (2560.0, 1440.0),
            (320.0, 240.0),
            // 箱 (66x62) より小さい極端な窓
            (50.0, 40.0),
        ]
        .into_iter()
        .map(|(w, h)| Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h)))
        .collect()
    }

    /// どの倍率・どの位置指定でも、ペットの矩形はビューポートの中に入る。
    #[test]
    fn ペットは常にビューポート内に収まる() {
        for vp in viewports() {
            for scale in [0.25_f32, 1.0, 2.0, 4.0] {
                let size = egui::vec2(BOX_W * scale, BOX_H * scale);
                let margin = EDGE_MARGIN * scale;
                // 憶えている位置の候補: 画面内 / 右外 / 下外 / 左上外 / 遠方
                let wants = [
                    None,
                    Some(egui::pos2(10.0, 10.0)),
                    Some(egui::pos2(vp.right() + 500.0, vp.bottom() + 500.0)),
                    Some(egui::pos2(-800.0, -800.0)),
                    Some(egui::pos2(vp.right() - 1.0, vp.bottom() - 1.0)),
                ];
                for want in wants {
                    for roam_x in [0.0_f32, 65.0, 130.0] {
                        let r = pet_rect(vp, size, want, roam_x, margin);
                        assert_eq!(r.size(), size, "箱の大きさは変えない");
                        // 収まる余地があるときは、余白ぶんも含めて内側に居る。
                        let fits = vp.width() >= size.x + margin * 2.0
                            && vp.height() >= size.y + margin * 2.0;
                        if fits {
                            assert!(
                                r.left() >= vp.left() + margin - 0.01
                                    && r.right() <= vp.right() - margin + 0.01
                                    && r.top() >= vp.top() + margin - 0.01
                                    && r.bottom() <= vp.bottom() - margin + 0.01,
                                "vp={vp:?} scale={scale} want={want:?}: {r:?} がはみ出した"
                            );
                        } else {
                            // 箱の方が大きい窓では左上に寄せる (右下へ逃がさない)
                            assert!(r.left() >= vp.left() - 0.01 && r.top() >= vp.top() - 0.01);
                        }
                    }
                }
            }
        }
    }

    /// アンカー既定は右下寄り。うろうろの位相ぶんだけ左へ動く。
    #[test]
    fn アンカーは右下でうろうろは左へ動く() {
        let vp = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1400.0, 900.0));
        let size = egui::vec2(BOX_W, BOX_H);
        let a = pet_rect(vp, size, None, 0.0, EDGE_MARGIN);
        let b = pet_rect(vp, size, None, 130.0, EDGE_MARGIN);
        assert!(b.left() < a.left(), "うろうろで左へ動く");
        assert!(a.right() < vp.right(), "右端に触れない");
        assert!(a.bottom() < vp.bottom(), "下端に触れない");
    }

    /// 窓を狭めると、憶えている位置ごと画面内へ引き戻される。
    #[test]
    fn 狭い窓では憶えた位置ごと引き戻される() {
        let wide = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1600.0, 1000.0));
        let size = egui::vec2(BOX_W, BOX_H);
        // 広い窓の右下でドラッグして憶えた位置
        let dragged = pet_rect(
            wide,
            size,
            Some(egui::pos2(1500.0, 900.0)),
            0.0,
            EDGE_MARGIN,
        )
        .min;
        let narrow = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 700.0));
        let r = pet_rect(narrow, size, Some(dragged), 0.0, EDGE_MARGIN);
        assert!(r.right() <= narrow.right() - EDGE_MARGIN + 0.01, "{r:?}");
        assert!(r.bottom() <= narrow.bottom() - EDGE_MARGIN + 0.01, "{r:?}");
    }

    /// 原点がずれたビューポート (マルチディスプレイ) でも中に入る。
    #[test]
    fn 原点がずれた画面でも中に入る() {
        let vp = Rect::from_min_size(egui::pos2(-1920.0, 70.0), egui::vec2(1200.0, 800.0));
        let size = egui::vec2(BOX_W, BOX_H);
        let r = pet_rect(vp, size, None, 0.0, EDGE_MARGIN);
        assert!(vp.contains_rect(r), "vp={vp:?} r={r:?}");
        let r2 = pet_rect(vp, size, Some(egui::pos2(0.0, 0.0)), 0.0, EDGE_MARGIN);
        assert!(vp.contains_rect(r2), "vp={vp:?} r2={r2:?}");
    }
}

/// **アイドルで本当に静止するか** — 設計原則 3 のリリースゲートを
/// 実時間を待たずに固定する検査。時計は `RawInput::time` で注入する。
#[cfg(test)]
mod sleep_tests {
    use super::*;

    fn idle_input() -> PetInput {
        PetInput {
            working: 0,
            attention: 0,
            recent_success: false,
            recent_error: false,
            variant: PetVariant::Blocky,
            scale: 1.0,
            free_roam: true,
            sleep_enabled: true,
        }
    }

    /// 時計を注入してペットを描く headless ハーネス。**実時間を 1 秒も待たない。**
    struct Harness {
        ctx: egui::Context,
        theme: Theme,
        rt: PetRuntime,
        pos: Option<Pos2>,
        t: f64,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                ctx: egui::Context::default(),
                theme: crate::theme::by_name("zaivern-dark"),
                rt: PetRuntime::default(),
                pos: None,
                t: 1.0,
            }
        }

        /// `dt` 秒進めて 1 フレーム描く。返り値は egui が予約した再描画待ち
        /// (`Duration::MAX` = **1 枚も予約していない**)。
        fn frame(
            &mut self,
            input: &PetInput,
            dt: f64,
            events: Vec<egui::Event>,
        ) -> std::time::Duration {
            self.t += dt;
            let raw = egui::RawInput {
                time: Some(self.t),
                screen_rect: Some(Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(1200.0, 800.0),
                )),
                events,
                ..Default::default()
            };
            let Self {
                ctx,
                theme,
                rt,
                pos,
                ..
            } = self;
            let out = ctx.run(raw, |c| {
                draw(c, theme, input, pos, None, rt);
            });
            out.viewport_output
                .values()
                .map(|v| v.repaint_delay)
                .min()
                .unwrap_or(std::time::Duration::MAX)
        }
    }

    /// 溜まった要求を捨てて、以降の観測を自分のフレームだけに閉じる。
    fn drain() -> std::collections::BTreeMap<&'static str, u64> {
        crate::perf::test_sink::take()
    }

    /// **活動なしで時計を進めたら、`SLEEP_AFTER` を過ぎた時点で
    /// 再描画要求が完全に止まる。**
    ///
    /// 実時間を待たない (時計は `RawInput::time` で注入する)。
    /// 絶対時間ではなく「状態と回数」で線を引く — 予約が `None` に
    /// なったか / 何件出たか、だけを見る。
    #[test]
    fn 放置がsleep_afterを過ぎたら要求が止まる() {
        let input = idle_input();
        let mut h = Harness::new();
        let _ = drain();

        // SLEEP_AFTER までは動いている (= 止まって見えない)
        let mut before = 0u64;
        while h.t - h.rt.last_input_time < SLEEP_AFTER {
            h.frame(&input, 1.0, vec![]);
            before += drain().values().sum::<u64>();
        }
        assert!(before > 0, "眠るまでの間は動いているはず");

        // 越えたあとは 1 件も出ない
        for _ in 0..200 {
            let delay = h.frame(&input, 1.0, vec![]);
            let got = drain();
            assert!(got.is_empty(), "静止後に要求が出た: {got:?}");
            assert_eq!(
                delay,
                std::time::Duration::MAX,
                "静止後にフレームが予約された"
            );
        }
    }

    /// **止まったあとに入力があれば、その場で起きる。**
    /// (寝たまま反応しなくなっていないこと)
    #[test]
    fn 入力があれば即座に起きる() {
        let input = idle_input();
        let mut h = Harness::new();
        while h.t - h.rt.last_input_time < SLEEP_AFTER + 5.0 {
            h.frame(&input, 1.0, vec![]);
        }
        let _ = drain();
        assert_eq!(h.frame(&input, 1.0, vec![]), std::time::Duration::MAX);
        assert!(drain().is_empty());

        // ポインタを動かす。`delta` は**前フレームとの差**なので、窓の外から
        // 入ってきた最初の 1 件は差が取れない (実機のマウスは連続でイベントを
        // 出すので、動かし始めの 1 フレーム以内に必ず差が出る)。
        h.frame(
            &input,
            0.1,
            vec![egui::Event::PointerMoved(egui::pos2(400.0, 300.0))],
        );
        let _ = drain();
        let moved = h.frame(
            &input,
            0.1,
            vec![egui::Event::PointerMoved(egui::pos2(420.0, 300.0))],
        );
        let got = drain();
        assert!(!got.is_empty(), "入力があったのに起きない");
        assert!(
            moved < std::time::Duration::from_millis(600),
            "起床が遅い: {moved:?}"
        );

        // ホイールだけでも起きる (端末をスクロールしている間は寝ない)
        while h.t - h.rt.last_input_time < SLEEP_AFTER + 1.0 {
            h.frame(&input, 1.0, vec![]);
        }
        let _ = drain();
        assert_eq!(h.frame(&input, 1.0, vec![]), std::time::Duration::MAX);
        let _ = drain();
        h.frame(
            &input,
            0.1,
            vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(0.0, -3.0),
                modifiers: egui::Modifiers::default(),
            }],
        );
        assert!(!drain().is_empty(), "ホイールで起きない");
    }

    /// **アイドルを N フレーム続けたあと、`perf` の内訳が空になる。**
    ///
    /// `perf::repaint_ranking()` が空 = 「damage 駆動のみ」が守れている
    /// ことの、集計側から見た同じ事実。
    #[test]
    fn 静止後はperfの内訳が空になる() {
        let input = idle_input();
        let mut h = Harness::new();
        while h.t - h.rt.last_input_time < SLEEP_AFTER + 1.0 {
            h.frame(&input, 1.0, vec![]);
        }
        let _ = drain();

        let mut st = crate::perf::FrameStats::new();
        for _ in 0..600 {
            h.frame(&input, 1.0, vec![]);
            st.record_frame(1_000, true, drain());
        }
        assert_eq!(st.idle_frames, 600);
        assert!(
            st.repaint_ranking().is_empty(),
            "静止しているのに出所が残っている: {:?}",
            st.repaint_ranking()
        );
        assert_eq!(st.idle_repaints(), 0);
        assert!(st.always_on_sources().is_empty());
    }

    /// **睡眠を切っていても止まる。** 設定で設計原則 3 を外せない。
    #[test]
    fn 睡眠を切っていても放置すれば止まる() {
        let mut input = idle_input();
        input.sleep_enabled = false;
        let mut h = Harness::new();
        while h.t - h.rt.last_input_time < SLEEP_AFTER + 1.0 {
            h.frame(&input, 1.0, vec![]);
        }
        let _ = drain();
        for _ in 0..100 {
            h.frame(&input, 1.0, vec![]);
        }
        let got = drain();
        assert!(got.is_empty(), "pet_sleep=false で回り続けている: {got:?}");
    }

    /// **エージェントが走っている間は止めない** (固まって見えてはいけない)。
    #[test]
    fn 作業中は放置しても止まらない() {
        let mut input = idle_input();
        input.working = 2;
        let mut h = Harness::new();
        for _ in 0..(SLEEP_AFTER as usize + 60) {
            h.frame(&input, 1.0, vec![]);
        }
        let got = drain();
        assert!(!got.is_empty(), "作業中なのに止まった");
        assert!(
            got.keys().all(|k| k.starts_with("pet_anim.")),
            "出所タグが割れていない: {got:?}"
        );
    }

    /// **止まる前後で、要求の総量が上限を超えない。**
    ///
    /// 絶対時間ではなく**件数**で線を引く。ポインタが 1 回動くたびに
    /// この回数だけ描き直す、という意味なのでここが番人になる
    /// (直す前は前面で 500 件だった)。
    #[test]
    fn 一度の入力で出る要求の総量に上限がある() {
        let input = idle_input();
        let mut h = Harness::new();
        let _ = drain();
        // **予約した刻みで次のフレームが来る** (egui の実挙動) を模して回す。
        let mut total = 0u64;
        let mut dt = 0.0;
        let mut frames = 0u64;
        loop {
            let delay = h.frame(&input, dt, vec![]);
            total += drain().values().sum::<u64>();
            frames += 1;
            assert!(frames < 5_000, "静止しない");
            if delay == std::time::Duration::MAX {
                break;
            }
            // egui は起動直後など「いますぐ」を返すことがある。0 のままだと
            // 時計が進まないので、実機の上限 (60fps) で下から押さえる。
            dt = delay.as_secs_f64().max(1.0 / 60.0);
        }
        // 前面: Idle 500ms × 20s = 40 件 + Dozing 1000ms × 40s = 40 件 → 80 件。
        // 直す前は Idle 80ms × 20s = 250 + Dozing 160ms × 40s = 250 → 500 件だった。
        assert!(
            total <= 120,
            "1 度の入力で {total} 件も要求している (上限 120, 実測 {frames} フレーム)"
        );
        assert!(total > 0, "そもそも動いていない");
        // 静止までにかかる時間は SLEEP_AFTER のまま (見た目の挙動は変えない)
        let elapsed = h.t - h.rt.last_input_time;
        assert!(
            (SLEEP_AFTER..SLEEP_AFTER + 2.0).contains(&elapsed),
            "静止まで {elapsed}s (SLEEP_AFTER={SLEEP_AFTER}s のはず)"
        );
    }
}
