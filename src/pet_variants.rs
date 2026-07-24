//! pet_variants.rs — デスクトップペットの代替キャラクター集（ベクター描画）
//!
//! clawd-on-desk 風のキャラクターを 3 種類提供する。
//! - `draw_crab`  : ピクセルガニ「Clawd」…オレンジのカニ。ハサミと 6 本脚
//! - `draw_cat`   : 三毛猫「Calico」…白地に橙と黒のぶち。しっぽと三角耳
//! - `draw_cloud` : くもの子「Cloudling」…ふわふわ浮かぶ雲。接地しない
//!
//! いずれも `crate::pet` の `PetState` / `DrawParams` の契約に従う。
//! `rect` はペット枠 (66x62 * scale)。全ジオメトリは `p.scale` に追従し、
//! `p.flip_x` で中心を軸に左右反転する（視線 `eye_look` は画面座標のため反転しない）。

use eframe::egui::{pos2, vec2, Color32, Painter, Pos2, Rect, Rounding, Shape, Stroke, Vec2};

use crate::pet::{DrawParams, PetState};

// ────────────────────────────────────────────────────────────────
// 共通ヘルパー
// ────────────────────────────────────────────────────────────────

/// 目の表示モード（状態とまばたきから決定）
#[derive(Clone, Copy, PartialEq)]
enum EyeMode {
    /// 開眼（瞳がカーソル追従）
    Open,
    /// 半目（うとうと）
    Half,
    /// 閉眼（就寝・まばたき）
    Closed,
    /// ×目（エラー）
    Cross,
    /// にっこり ^ ^（ハッピー）
    Arc,
}

/// 状態＋まばたきフラグ → 目モード
fn eye_mode(state: PetState, blink: bool) -> EyeMode {
    match state {
        PetState::Sleeping => EyeMode::Closed,
        PetState::Dozing => EyeMode::Half,
        PetState::Error => EyeMode::Cross,
        PetState::Happy => EyeMode::Arc,
        _ => {
            if blink {
                EyeMode::Closed
            } else {
                EyeMode::Open
            }
        }
    }
}

/// 状態ごとの体の動き（基準単位系。描画時に scale 倍する）
struct Motion {
    /// 横シェイク量
    dx: f32,
    /// 縦オフセット（負で上＝ホップ）
    dy: f32,
    /// 呼吸などの縦スケール（1.0 が基準）
    squash: f32,
    /// 手足アニメの速度倍率
    speed: f32,
}

fn state_motion(state: PetState, t: f64) -> Motion {
    let tf = t as f32;
    match state {
        // 就寝: ゆっくり大きめの呼吸
        PetState::Sleeping => Motion {
            dx: 0.0,
            dy: 0.5,
            squash: 1.0 + 0.045 * (tf * 1.1).sin(),
            speed: 0.15,
        },
        // うとうと: 浅い呼吸
        PetState::Dozing => Motion {
            dx: 0.0,
            dy: 0.2,
            squash: 1.0 + 0.028 * (tf * 1.6).sin(),
            speed: 0.4,
        },
        // 作業中: 手足が速く動く（並列数が多いほどさらに速く）
        PetState::Working(n) => Motion {
            dx: 0.0,
            dy: 0.0,
            squash: 1.0 + 0.015 * (tf * 6.0).sin(),
            speed: 2.6 + 0.25 * n.min(5) as f32,
        },
        // ノリノリ: バウンスは pet.rs 側の bob が担う(ここで足すと二重ホップになる)
        PetState::Groove => Motion {
            dx: 0.0,
            dy: 0.0,
            squash: 1.0 + 0.05 * (tf * 6.0).sin(),
            speed: 1.6,
        },
        // 注目: 横に小刻みシェイク
        PetState::Attention => Motion {
            dx: (tf * 12.0).sin() * 1.6,
            dy: 0.0,
            squash: 1.0,
            speed: 1.2,
        },
        // ハッピー: ホップは pet.rs 側の bob が担う(ここで足すと二重ホップになる)
        PetState::Happy => Motion {
            dx: 0.0,
            dy: 0.0,
            squash: 1.0,
            speed: 1.5,
        },
        // エラー: しゅんと沈む
        PetState::Error => Motion {
            dx: 0.0,
            dy: 0.8,
            squash: 0.97,
            speed: 0.5,
        },
        // イライラ: 高速シェイク
        PetState::Annoyed => Motion {
            dx: (tf * 24.0).sin() * 2.0,
            dy: 0.0,
            squash: 1.0,
            speed: 2.0,
        },
        // 待機/散歩: 通常＋かすかな呼吸
        PetState::Idle | PetState::Roam => Motion {
            dx: 0.0,
            dy: 0.0,
            squash: 1.0 + 0.015 * (tf * 2.2).sin(),
            speed: 1.0,
        },
    }
}

/// 接地影の楕円（カニ・ネコ用。雲は浮遊するので描かない）
/// `lift` はホップ量（基準単位・正で浮いている）。浮くほど影が小さくなる。
fn ground_shadow(painter: &Painter, rect: Rect, u: f32, lift: f32, dragging: bool) {
    let k = if dragging {
        0.55
    } else {
        (1.0 - lift * 0.06).clamp(0.5, 1.0)
    };
    painter.add(Shape::ellipse_filled(
        pos2(rect.center().x, rect.bottom() - 3.0 * u),
        vec2(15.0 * u * k, 3.0 * u * k),
        Color32::from_black_alpha(55),
    ));
}

/// 白目＋瞳タイプの目を 1 個描く（カニ・ネコ用）
/// `rx`/`ry` は白目の半径（px）。`look` は瞳のカーソル追従オフセット。
fn draw_ball_eye(
    painter: &Painter,
    c: Pos2,
    rx: f32,
    ry: f32,
    mode: EyeMode,
    look: Vec2,
    ink: Color32,
    u: f32,
) {
    let lw = 1.4 * u;
    match mode {
        EyeMode::Closed => {
            // 閉眼 → 水平線
            painter.line_segment([c + vec2(-rx, 0.0), c + vec2(rx, 0.0)], Stroke::new(lw, ink));
        }
        EyeMode::Cross => {
            // ×目
            let r = rx * 0.9;
            painter.line_segment([c + vec2(-r, -r), c + vec2(r, r)], Stroke::new(lw, ink));
            painter.line_segment([c + vec2(-r, r), c + vec2(r, -r)], Stroke::new(lw, ink));
        }
        EyeMode::Arc => {
            // にっこり ^ の山型アーク
            painter.add(Shape::line(
                vec![
                    c + vec2(-rx, ry * 0.4),
                    c + vec2(0.0, -ry * 0.5),
                    c + vec2(rx, ry * 0.4),
                ],
                Stroke::new(lw, ink),
            ));
        }
        EyeMode::Open | EyeMode::Half => {
            // 半目は白目を下半分だけにして上にまぶた線を引く
            let (cc, ry2) = if mode == EyeMode::Half {
                (c + vec2(0.0, ry * 0.25), ry * 0.55)
            } else {
                (c, ry)
            };
            painter.add(Shape::ellipse_filled(cc, vec2(rx, ry2), Color32::WHITE));
            painter.add(Shape::ellipse_stroke(
                cc,
                vec2(rx, ry2),
                Stroke::new(1.0 * u, ink),
            ));
            if mode == EyeMode::Half {
                painter.line_segment(
                    [cc + vec2(-rx, -ry2), cc + vec2(rx, -ry2)],
                    Stroke::new(lw, ink),
                );
            }
            // 瞳: eye_look に追従（白目からはみ出さない範囲でクランプ）
            let off = vec2(
                look.x.clamp(-rx * 0.4, rx * 0.4),
                look.y.clamp(-ry2 * 0.35, ry2 * 0.35),
            );
            painter.circle_filled(cc + off, ry2 * 0.5, ink);
        }
    }
}

/// 怒り眉（内側が下がる「ハ」の字を上下逆にした形）
fn angry_brows(painter: &Painter, le: Pos2, re: Pos2, ink: Color32, u: f32) {
    let s = Stroke::new(1.4 * u, ink);
    painter.line_segment([le + vec2(-3.0 * u, -4.5 * u), le + vec2(2.5 * u, -2.6 * u)], s);
    painter.line_segment([re + vec2(-2.5 * u, -2.6 * u), re + vec2(3.0 * u, -4.5 * u)], s);
}

/// 口のアーク。`depth > 0` で笑い(∪)、`depth < 0` でへの字(∩)。
fn mouth_arc(painter: &Painter, c: Pos2, w: f32, depth: f32, stroke: Stroke) {
    let n = 7;
    let mut pts = Vec::with_capacity(n);
    for i in 0..n {
        let x = i as f32 / (n - 1) as f32 * 2.0 - 1.0; // -1..1
        pts.push(c + vec2(x * w * 0.5, depth * (1.0 - x * x)));
    }
    painter.add(Shape::line(pts, stroke));
}

/// 状態ごとの口の曲がり量（基準単位）
fn mouth_depth(state: PetState) -> f32 {
    match state {
        PetState::Error | PetState::Annoyed => -2.0,
        PetState::Happy | PetState::Groove => 2.8,
        PetState::Sleeping | PetState::Dozing => 1.0,
        _ => 1.8,
    }
}

/// 三角形の内側に縮小コピーを作る（耳の内側ピンク用）
fn inner_tri(pts: &[Pos2; 3], k: f32) -> Vec<Pos2> {
    let cx = (pts[0].x + pts[1].x + pts[2].x) / 3.0;
    let cy = (pts[0].y + pts[1].y + pts[2].y) / 3.0;
    pts.iter()
        .map(|p| pos2(cx + (p.x - cx) * k, cy + (p.y - cy) * k))
        .collect()
}

// ────────────────────────────────────────────────────────────────
// 1. カニ「Clawd」
// ────────────────────────────────────────────────────────────────

pub fn draw_crab(painter: &Painter, rect: Rect, t: f64, state: PetState, p: &DrawParams) {
    let u = p.scale;
    let tf = t as f32;
    let m = state_motion(state, t);
    let dir = if p.flip_x { -1.0 } else { 1.0 };

    const BODY: Color32 = Color32::from_rgb(0xF7, 0x5C, 0x1A);
    const OUTLINE: Color32 = Color32::from_rgb(0xE0, 0x49, 0x08);
    let cheek = Color32::from_rgba_unmultiplied(255, 130, 150, 170);

    // 体の基準中心（bob と状態モーションを合成）
    let dy_px = m.dy * u + p.bob;
    let c = pos2(rect.center().x + m.dx * u * dir, rect.center().y + dy_px);
    // 反転対応の座標変換（dx/dy は基準単位・中心からのオフセット）
    let q = |dx: f32, dy: f32| pos2(c.x + dx * dir * u, c.y + dy * u);

    // ── 接地影 ──
    let lift = (-dy_px / u.max(0.001)).max(0.0);
    ground_shadow(painter, rect, u, lift, p.dragging);

    // ── 脚（片側 3 本。leg_t で交互にスイング。ドラッグ中はだらんと下がる） ──
    let dangle = if p.dragging { 2.5 } else { 0.0 };
    for side in [-1.0f32, 1.0] {
        for i in 0..3 {
            let fi = i as f32;
            let alt = if i % 2 == 0 { 1.0 } else { -1.0 };
            let sway = p.leg_t * 2.2 * alt * m.speed.min(2.0)
                + (tf * m.speed * 5.0 + fi * 2.1).sin() * 0.35 * m.speed;
            let root = q(side * (10.0 + fi * 2.0), 10.0 - fi * 4.0);
            let tip = q(side * (19.0 + fi * 2.5) + sway, 22.0 - fi * 4.5 + dangle);
            painter.line_segment([root, tip], Stroke::new(2.0 * u, OUTLINE));
        }
    }

    // ── 腕とハサミ（wave でスイング、Working で高速チョキチョキ） ──
    let working = matches!(state, PetState::Working(_));
    let raise = if state == PetState::Groove { 4.0 } else { 0.0 };
    for side in [-1.0f32, 1.0] {
        let mut cy2 = -7.0 + p.wave * 3.5 * side - raise;
        if working {
            cy2 += (tf * 10.0 + side).sin() * 1.2;
        }
        let cbx = side * 23.0;
        let claw = q(cbx, cy2);
        // 腕
        painter.line_segment([q(side * 13.0, -2.0), claw], Stroke::new(2.2 * u, OUTLINE));
        // ハサミ本体
        painter.circle_filled(claw, 7.0 * u, BODY);
        painter.circle_stroke(claw, 7.0 * u, Stroke::new(1.6 * u, OUTLINE));
        // ハサミの切れ込み（Working 中は開閉が高速で振動）
        let open = if working {
            0.55 + 0.3 * (tf * 10.0).sin()
        } else {
            0.35
        };
        let e1 = q(cbx + side * 6.5 * open.cos(), cy2 - 6.5 * open.sin());
        let e2 = q(cbx + side * 6.5 * open.cos(), cy2 + 6.5 * open.sin());
        painter.line_segment([claw, e1], Stroke::new(1.6 * u, OUTLINE));
        painter.line_segment([claw, e2], Stroke::new(1.6 * u, OUTLINE));
    }

    // ── 甲羅（丸い体。呼吸で縦に伸縮） ──
    let body_c = q(0.0, 4.0);
    painter.add(Shape::ellipse_filled(
        body_c,
        vec2(18.0 * u, 18.0 * u * m.squash),
        BODY,
    ));
    painter.add(Shape::ellipse_stroke(
        body_c,
        vec2(18.0 * u, 18.0 * u * m.squash),
        Stroke::new(2.0 * u, OUTLINE),
    ));

    // ── 目（ストーク付き。白目 r5 相当＋瞳がカーソル追従） ──
    let eyes = eye_mode(state, p.blink);
    let (le, re) = (q(-7.5, -22.5), q(7.5, -22.5));
    for side in [-1.0f32, 1.0] {
        painter.line_segment(
            [q(side * 5.5, -10.0), q(side * 7.5, -20.0)],
            Stroke::new(2.0 * u, OUTLINE),
        );
    }
    draw_ball_eye(painter, le, 4.8 * u, 4.8 * u, eyes, p.eye_look, Color32::BLACK, u);
    draw_ball_eye(painter, re, 4.8 * u, 4.8 * u, eyes, p.eye_look, Color32::BLACK, u);
    if state == PetState::Annoyed {
        angry_brows(painter, le, re, OUTLINE, u);
    }

    // ── ほっぺと口 ──
    painter.circle_filled(q(-9.0, 1.0), 2.4 * u, cheek);
    painter.circle_filled(q(9.0, 1.0), 2.4 * u, cheek);
    mouth_arc(
        painter,
        q(0.0, 6.0),
        9.0 * u,
        mouth_depth(state) * u,
        Stroke::new(1.6 * u, Color32::from_rgb(120, 40, 10)),
    );
}

// ────────────────────────────────────────────────────────────────
// 2. 三毛猫「Calico」
// ────────────────────────────────────────────────────────────────

pub fn draw_cat(painter: &Painter, rect: Rect, t: f64, state: PetState, p: &DrawParams) {
    let u = p.scale;
    let tf = t as f32;
    let m = state_motion(state, t);
    let dir = if p.flip_x { -1.0 } else { 1.0 };

    const FUR: Color32 = Color32::from_rgb(255, 250, 242);
    const INK: Color32 = Color32::from_rgb(80, 70, 64);
    const ORANGE: Color32 = Color32::from_rgb(240, 150, 60);
    const DARK: Color32 = Color32::from_rgb(58, 54, 52);
    const PINK: Color32 = Color32::from_rgb(245, 150, 165);
    let cheek = Color32::from_rgba_unmultiplied(250, 150, 165, 150);

    let dy_px = m.dy * u + p.bob;
    let c = pos2(rect.center().x + m.dx * u * dir, rect.center().y + dy_px);
    let q = |dx: f32, dy: f32| pos2(c.x + dx * dir * u, c.y + dy * u);

    // ── 接地影 ──
    let lift = (-dy_px / u.max(0.001)).max(0.0);
    ground_shadow(painter, rect, u, lift, p.dragging);

    // ── しっぽ（wave で揺れ、Working 中は高速フリック。体の後ろに描く） ──
    let flick = if matches!(state, PetState::Working(_)) {
        (tf * 9.0).sin() * 2.5
    } else {
        0.0
    };
    let base = [
        (14.0, 11.0),
        (18.5, 9.0),
        (21.5, 4.5),
        (22.0, -1.0),
        (20.0, -6.0),
    ];
    let tail: Vec<Pos2> = base
        .iter()
        .enumerate()
        .map(|(i, &(x, y))| {
            let k = i as f32 / 4.0;
            q(x + (p.wave * 3.0 + flick) * k, y)
        })
        .collect();
    let tail_tip = tail[4];
    painter.add(Shape::line(tail, Stroke::new(2.6 * u, ORANGE)));
    // しっぽの先だけ黒（三毛らしさ）
    painter.circle_filled(tail_tip, 1.8 * u, DARK);

    // ── 胴体（白い角丸長方形。呼吸で縦に伸縮） ──
    let body = Rect::from_center_size(q(0.0, 3.0), vec2(30.0 * u, 28.0 * u * m.squash));
    painter.rect_filled(body, Rounding::same(9.0 * u), FUR);
    // ぶち模様（橙＋黒。胴体の内側に収める）
    painter.circle_filled(q(-7.0, -4.0), 5.0 * u, ORANGE);
    painter.circle_filled(q(8.0, -6.0), 3.8 * u, DARK);
    painter.circle_filled(q(6.0, 10.0), 4.2 * u, ORANGE);
    painter.rect_stroke(body, Rounding::same(9.0 * u), Stroke::new(1.6 * u, INK));

    // ── 三角耳（wave で先端がわずかに揺れる。左=橙 / 右=黒、内側ピンク） ──
    let l_ear = [
        q(-14.0, -8.0),
        q(-11.0 + p.wave * 0.8, -21.0),
        q(-4.0, -10.0),
    ];
    let r_ear = [
        q(4.0, -10.0),
        q(11.0 - p.wave * 0.8, -21.0),
        q(14.0, -8.0),
    ];
    painter.add(Shape::convex_polygon(
        l_ear.to_vec(),
        ORANGE,
        Stroke::new(1.4 * u, INK),
    ));
    painter.add(Shape::convex_polygon(
        r_ear.to_vec(),
        DARK,
        Stroke::new(1.4 * u, INK),
    ));
    painter.add(Shape::convex_polygon(inner_tri(&l_ear, 0.45), PINK, Stroke::NONE));
    painter.add(Shape::convex_polygon(inner_tri(&r_ear, 0.45), PINK, Stroke::NONE));

    // ── 目（アーモンド形。瞳がカーソル追従） ──
    let eyes = eye_mode(state, p.blink);
    let (le, re) = (q(-6.0, -2.0), q(6.0, -2.0));
    draw_ball_eye(painter, le, 3.4 * u, 2.6 * u, eyes, p.eye_look, INK, u);
    draw_ball_eye(painter, re, 3.4 * u, 2.6 * u, eyes, p.eye_look, INK, u);
    if state == PetState::Annoyed {
        angry_brows(painter, le, re, INK, u);
    }

    // ── 鼻（ピンクの逆三角）と口 ──
    painter.add(Shape::convex_polygon(
        vec![q(-1.6, 2.0), q(1.6, 2.0), q(0.0, 4.2)],
        PINK,
        Stroke::NONE,
    ));
    mouth_arc(
        painter,
        q(0.0, 6.5),
        7.0 * u,
        mouth_depth(state) * 0.8 * u,
        Stroke::new(1.3 * u, INK),
    );

    // ── ヒゲ（左右 3 本ずつ） ──
    let whisker = Stroke::new(1.0 * u, Color32::from_rgb(130, 120, 110));
    for side in [-1.0f32, 1.0] {
        painter.line_segment([q(side * 13.0, 2.0), q(side * 21.0, 0.0)], whisker);
        painter.line_segment([q(side * 13.0, 4.0), q(side * 21.5, 4.0)], whisker);
        painter.line_segment([q(side * 13.0, 6.0), q(side * 21.0, 8.0)], whisker);
    }

    // ── ほっぺと前足（leg_t でちょこちょこ動く。ドラッグ中は下にだらん） ──
    painter.circle_filled(q(-10.5, 3.0), 2.2 * u, cheek);
    painter.circle_filled(q(10.5, 3.0), 2.2 * u, cheek);
    let paw_dy = if p.dragging { 1.5 } else { 0.0 };
    for side in [-1.0f32, 1.0] {
        let px = side * 5.0 + p.leg_t * 1.5 * side;
        painter.circle_filled(q(px, 15.5 + paw_dy), 2.6 * u, FUR);
        painter.circle_stroke(q(px, 15.5 + paw_dy), 2.6 * u, Stroke::new(1.0 * u, INK));
    }
}

// ────────────────────────────────────────────────────────────────
// 3. くもの子「Cloudling」
// ────────────────────────────────────────────────────────────────

pub fn draw_cloud(painter: &Painter, rect: Rect, t: f64, state: PetState, p: &DrawParams) {
    let u = p.scale;
    let tf = t as f32;
    let m = state_motion(state, t);
    let dir = if p.flip_x { -1.0 } else { 1.0 };

    const UNDER: Color32 = Color32::from_rgb(0xD8, 0xD8, 0xD8);
    let puff = if state == PetState::Error {
        // エラー時は雨雲らしくグレー寄りに
        Color32::from_rgb(225, 228, 235)
    } else {
        Color32::from_rgb(250, 250, 252)
    };
    const INK: Color32 = Color32::from_rgb(70, 70, 80);
    let cheek = Color32::from_rgba_unmultiplied(250, 150, 160, 160);

    // ── 浮遊: bob を強めに反映し、常時ゆらゆら漂う（接地しない） ──
    let mut dy_px = p.bob * 1.8 + ((tf * 1.3).sin() * 2.0 + m.dy) * u;
    if matches!(state, PetState::Working(_)) {
        // 作業中は小刻みに速くバウンド
        dy_px += (tf * 9.0).sin() * 1.5 * u;
    }
    let c = pos2(rect.center().x + m.dx * u * dir, rect.center().y + dy_px);
    let q = |dx: f32, dy: f32| pos2(c.x + dx * dir * u, c.y + dy * u);

    // ── ずんぐり足（浮遊中もぷらぷら。leg_t でスイング、ドラッグ中はだらん） ──
    let dangle = if p.dragging { 1.8 } else { 0.0 };
    for side in [-1.0f32, 1.0] {
        let fx = side * 5.5 + p.leg_t * 1.2 * side;
        painter.add(Shape::ellipse_filled(
            q(fx, 13.5 + dangle),
            vec2(3.2 * u, 2.2 * u),
            Color32::from_rgb(233, 233, 239),
        ));
        painter.add(Shape::ellipse_stroke(
            q(fx, 13.5 + dangle),
            vec2(3.2 * u, 2.2 * u),
            Stroke::new(1.0 * u, UNDER),
        ));
    }

    // ── もこもこ本体（下面グレー→白の 4 パフ。呼吸で縦に伸縮） ──
    let puffs = [
        (0.0, -3.0, 12.0),
        (-10.0, 1.0, 9.0),
        (10.0, 1.0, 9.0),
        (0.0, 4.0, 10.0),
    ];
    // 下面の影レイヤー（少し下にずらしたグレー）
    for &(x, y, r) in &puffs {
        painter.add(Shape::ellipse_filled(
            q(x, y + 2.0),
            vec2(r * u, r * u * m.squash),
            UNDER,
        ));
    }
    // 本体レイヤー
    for &(x, y, r) in &puffs {
        painter.add(Shape::ellipse_filled(
            q(x, y),
            vec2(r * u, r * u * m.squash),
            puff,
        ));
    }

    // ── エラー時: 雨粒が 3 滴パラパラ落ちる ──
    if state == PetState::Error {
        let rain = Stroke::new(1.5 * u, Color32::from_rgb(110, 160, 220));
        for k in 0..3 {
            let fk = k as f32;
            let x = -7.0 + 7.0 * fk;
            let fall = (tf * 0.8 + fk * 0.37).rem_euclid(1.0) * 10.0;
            painter.line_segment([q(x, 16.0 + fall), q(x, 19.5 + fall)], rain);
        }
    }

    // ── 顔（点目＋ほっぺ＋小さな口） ──
    let eyes = eye_mode(state, p.blink);
    let (le, re) = (q(-4.5, -2.0), q(4.5, -2.0));
    for &e in &[le, re] {
        match eyes {
            EyeMode::Open => {
                // 点目もカーソル追従
                let off = vec2(
                    p.eye_look.x.clamp(-1.5 * u, 1.5 * u),
                    p.eye_look.y.clamp(-1.2 * u, 1.2 * u),
                );
                painter.circle_filled(e + off, 1.7 * u, INK);
            }
            EyeMode::Half => {
                painter.line_segment(
                    [e + vec2(-1.8 * u, 0.0), e + vec2(1.8 * u, 0.0)],
                    Stroke::new(1.6 * u, INK),
                );
                painter.circle_filled(e + vec2(0.0, 0.8 * u), 0.9 * u, INK);
            }
            EyeMode::Closed => {
                painter.line_segment(
                    [e + vec2(-2.0 * u, 0.0), e + vec2(2.0 * u, 0.0)],
                    Stroke::new(1.4 * u, INK),
                );
            }
            EyeMode::Cross => {
                let r = 1.8 * u;
                painter.line_segment([e + vec2(-r, -r), e + vec2(r, r)], Stroke::new(1.3 * u, INK));
                painter.line_segment([e + vec2(-r, r), e + vec2(r, -r)], Stroke::new(1.3 * u, INK));
            }
            EyeMode::Arc => {
                painter.add(Shape::line(
                    vec![
                        e + vec2(-2.0 * u, 0.8 * u),
                        e + vec2(0.0, -u),
                        e + vec2(2.0 * u, 0.8 * u),
                    ],
                    Stroke::new(1.3 * u, INK),
                ));
            }
        }
    }
    if state == PetState::Annoyed {
        angry_brows(painter, le, re, INK, u);
    }
    painter.circle_filled(q(-8.0, 1.5), 2.0 * u, cheek);
    painter.circle_filled(q(8.0, 1.5), 2.0 * u, cheek);
    mouth_arc(
        painter,
        q(0.0, 3.5),
        5.0 * u,
        mouth_depth(state) * 0.6 * u,
        Stroke::new(1.2 * u, INK),
    );
}
