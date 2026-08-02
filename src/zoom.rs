//! ズーム (拡大 / 縮小) の純粋ロジック。
//!
//! 「どれだけ拡大するか」の判断だけをここに置き、egui にも `Config` にも
//! 依存しない。段の刻み・上下限・端数の丸めはテーブルテストで固定してある
//! ので、UI 側 (app.rs) は「1 段上げる / 下げる / 戻す」を呼ぶだけでよい。
//!
//! 対象は 2 つ。**混ぜない**:
//!
//! 1. **画面全体** (`window_*`) — egui の `zoom_factor` を動かす。
//!    サイドバー・タブ・端末・ステータスバーまで含めて全部が拡大する
//!    (VS Code の ⌘+ / ⌘- / ⌘0 と同じ)。
//! 2. **ファイル単位** (`file_*`) — そのタブの本文フォントだけを動かす。
//!    基準サイズ (`config.editor_font_size`) からの **段数 (pt)** で持つので、
//!    基準を変えても各ファイルの相対的な大きさは保たれる。

/// 画面全体ズームの段 (倍率)。VS Code と同じく 100% を中心に上下へ広げる。
///
/// **昇順・重複なし** であること。`window_step` はこの表を前後に舐めるだけ
/// なので、順序が崩れると 1 段の移動が飛ぶ。`window_levels_are_sorted` が守る。
pub const WINDOW_LEVELS: [f32; 15] = [
    0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 1.1, 1.2, 1.3, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0,
];

/// 画面全体ズームの下限 / 上限 (= 段表の両端)。
pub const WINDOW_MIN: f32 = WINDOW_LEVELS[0];
pub const WINDOW_MAX: f32 = WINDOW_LEVELS[WINDOW_LEVELS.len() - 1];

/// ファイル単位ズームで許すフォントサイズ (pt)。`config.editor_font_size`
/// のクランプ範囲と同じにしてある (これを外すと保存された基準サイズを
/// 開いた瞬間に段が食い違う)。
pub const FILE_FONT_MIN: f32 = 8.0;
pub const FILE_FONT_MAX: f32 = 32.0;

/// 異常値 (NaN / 無限 / 非正) を等倍へ丸める。設定ファイルの手書きや
/// 壊れた state.toml で NaN が入っても、ここで必ず有限値になる。
pub fn sanitize_window(z: f32) -> f32 {
    if !z.is_finite() || z <= 0.0 {
        return 1.0;
    }
    z.clamp(WINDOW_MIN, WINDOW_MAX)
}

/// 画面全体ズームを `dir` 段動かす (正=拡大 / 負=縮小 / 0=そのまま)。
///
/// 現在値が段の途中 (config で 1.15 を手書きした等) でも、
/// 「次に大きい段 / 次に小さい段」へ正しく着地する。
pub fn window_step(current: f32, dir: i32) -> f32 {
    let cur = sanitize_window(current);
    if dir == 0 {
        return cur;
    }
    // 誤差でその場に留まらないよう、比較には少しだけ余裕を持たせる。
    const EPS: f32 = 1e-4;
    let mut z = cur;
    for _ in 0..dir.unsigned_abs().min(WINDOW_LEVELS.len() as u32) {
        z = if dir > 0 {
            WINDOW_LEVELS
                .iter()
                .copied()
                .find(|l| *l > z + EPS)
                .unwrap_or(WINDOW_MAX)
        } else {
            WINDOW_LEVELS
                .iter()
                .copied()
                .rev()
                .find(|l| *l < z - EPS)
                .unwrap_or(WINDOW_MIN)
        };
    }
    z
}

/// ステータスバー / メニュー用のラベル ("125%")。
pub fn percent_label(z: f32) -> String {
    format!("{}%", (sanitize_window(z) * 100.0).round() as i32)
}

/// 基準サイズと段数から、そのファイルで実際に使うフォントサイズ (pt)。
pub fn file_font_size(base: f32, steps: i32) -> f32 {
    let base = if base.is_finite() && base > 0.0 {
        base
    } else {
        FILE_FONT_MIN
    };
    (base + steps as f32).clamp(FILE_FONT_MIN, FILE_FONT_MAX)
}

/// ファイル単位ズームの段数を `dir` だけ動かす。
///
/// 上下限に張り付いたあとも段数だけが増え続けると「20 回拡大 → 1 回縮小」で
/// 何も起きない (見た目が固まる) ため、**実際に効く範囲へ切り詰めて**返す。
pub fn file_step(base: f32, steps: i32, dir: i32) -> i32 {
    let base = if base.is_finite() && base > 0.0 {
        base
    } else {
        FILE_FONT_MIN
    };
    let lo = (FILE_FONT_MIN - base).ceil() as i32;
    let hi = (FILE_FONT_MAX - base).floor() as i32;
    // 基準サイズ自体が範囲外でも lo <= hi を保つ (clamp が panic しない)。
    let (lo, hi) = if lo <= hi { (lo, hi) } else { (0, 0) };
    steps.saturating_add(dir).clamp(lo, hi)
}

/// ファイル単位ズームのラベル ("+2pt" / "-1pt")。等倍のときは `None` —
/// ステータスバーに「±0」を常時出さないため (空白は作らない/常に 0 の
/// バッジは出さない、という UI 原則)。
pub fn file_label(steps: i32) -> Option<String> {
    if steps == 0 {
        None
    } else {
        Some(format!("{steps:+}pt"))
    }
}

/// ⌘ + ホイール / ピンチの連続値を「段」へ均す。
///
/// egui は ⌘+ホイールもトラックパッドのピンチも `zoom_delta()` (倍率) に
/// 集約する。倍率をそのまま段に直すと 1 ノッチで数段飛ぶので、対数で
/// 溜めてしきい値を跨いだぶんだけ返し、**端数は次のフレームへ持ち越す**
/// (取りこぼすとゆっくりしたピンチが効かなくなる)。
///
/// `accum` は呼び出し側が持つ溜め。ジェスチャが止んだフレーム
/// (`delta == 1.0`) では 0 に戻すので、次のジェスチャは必ず素の状態から始まる。
pub fn wheel_steps(accum: &mut f32, delta: f32) -> i32 {
    /// 1 段ぶんの対数量 (≒ 12% の拡大で 1 段)。
    const PER_STEP: f32 = 0.113_328_68; // ln(1.12)
    /// 1 フレームで進める上限。ホイールを勢いよく回しても飛びすぎない。
    const MAX_PER_FRAME: i32 = 4;

    if !delta.is_finite() || delta <= 0.0 || !accum.is_finite() {
        *accum = 0.0;
        return 0;
    }
    if (delta - 1.0).abs() <= f32::EPSILON {
        *accum = 0.0;
        return 0;
    }
    *accum += delta.ln();
    let steps = (*accum / PER_STEP).trunc() as i32;
    let steps = steps.clamp(-MAX_PER_FRAME, MAX_PER_FRAME);
    *accum -= steps as f32 * PER_STEP;
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 段表そのもの ----

    #[test]
    fn window_levels_are_sorted_and_unique() {
        for w in WINDOW_LEVELS.windows(2) {
            assert!(w[0] < w[1], "段表は昇順・重複なし: {w:?}");
        }
        assert!(
            WINDOW_LEVELS.contains(&1.0),
            "等倍が段表に無いと ⌘0 と往復できない"
        );
    }

    // ---- sanitize ----

    #[test]
    fn sanitize_maps_broken_values_to_one() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -2.0] {
            assert_eq!(sanitize_window(bad), 1.0, "壊れた値 {bad} は等倍へ");
        }
        assert_eq!(sanitize_window(10.0), WINDOW_MAX, "上限で頭打ち");
        assert_eq!(sanitize_window(0.01), WINDOW_MIN, "下限で底打ち");
        assert_eq!(sanitize_window(1.25), 1.25, "段の途中でもそのまま通す");
    }

    // ---- window_step ----

    #[test]
    fn window_step_walks_the_ladder() {
        let table: [(f32, i32, f32); 8] = [
            (1.0, 1, 1.1),
            (1.0, -1, 0.9),
            (1.0, 0, 1.0),
            (1.0, 3, 1.3),
            (1.0, -3, 0.7),
            (WINDOW_MAX, 1, WINDOW_MAX),
            (WINDOW_MIN, -1, WINDOW_MIN),
            (1.15, 1, 1.2), // 段の途中からは「次の段」へ
        ];
        for (from, dir, want) in table {
            let got = window_step(from, dir);
            assert!(
                (got - want).abs() < 1e-5,
                "window_step({from}, {dir}) = {got}, want {want}"
            );
        }
    }

    #[test]
    fn window_step_from_between_levels_goes_down_to_the_lower_level() {
        assert!((window_step(1.15, -1) - 1.1).abs() < 1e-5);
    }

    #[test]
    fn window_step_is_reversible_within_the_ladder() {
        let mut z = 1.0;
        for _ in 0..4 {
            z = window_step(z, 1);
        }
        for _ in 0..4 {
            z = window_step(z, -1);
        }
        assert!((z - 1.0).abs() < 1e-5, "同じ回数だけ戻せば等倍へ戻る: {z}");
    }

    #[test]
    fn window_step_survives_broken_input() {
        assert_eq!(window_step(f32::NAN, 1), 1.1);
        assert_eq!(window_step(0.0, -1), 0.9);
    }

    #[test]
    fn window_step_never_leaves_the_range() {
        let mut z = 1.0;
        for _ in 0..50 {
            z = window_step(z, 1);
            assert!((WINDOW_MIN..=WINDOW_MAX).contains(&z));
        }
        assert_eq!(z, WINDOW_MAX);
        for _ in 0..50 {
            z = window_step(z, -1);
            assert!((WINDOW_MIN..=WINDOW_MAX).contains(&z));
        }
        assert_eq!(z, WINDOW_MIN);
    }

    // ---- percent_label ----

    #[test]
    fn percent_label_is_readable() {
        assert_eq!(percent_label(1.0), "100%");
        assert_eq!(percent_label(1.25), "125%");
        assert_eq!(percent_label(0.5), "50%");
        assert_eq!(percent_label(f32::NAN), "100%");
    }

    // ---- file zoom ----

    #[test]
    fn file_font_size_clamps_to_editor_range() {
        assert_eq!(file_font_size(15.0, 0), 15.0);
        assert_eq!(file_font_size(15.0, 3), 18.0);
        assert_eq!(file_font_size(15.0, -3), 12.0);
        assert_eq!(file_font_size(15.0, 100), FILE_FONT_MAX);
        assert_eq!(file_font_size(15.0, -100), FILE_FONT_MIN);
        assert_eq!(file_font_size(f32::NAN, 0), FILE_FONT_MIN);
    }

    #[test]
    fn file_step_saturates_at_the_edges_and_comes_back_immediately() {
        let base = 15.0;
        let mut s = 0;
        for _ in 0..40 {
            s = file_step(base, s, 1);
        }
        assert_eq!(file_font_size(base, s), FILE_FONT_MAX, "上限に張り付く");
        // 上限で 40 回押しても段数は伸びていないので、1 回の縮小で必ず縮む
        let back = file_step(base, s, -1);
        assert!(
            file_font_size(base, back) < FILE_FONT_MAX,
            "上限のあと 1 回縮小したら必ず小さくなる"
        );
    }

    #[test]
    fn file_step_table() {
        let table: [(f32, i32, i32, i32); 6] = [
            (15.0, 0, 1, 1),
            (15.0, 0, -1, -1),
            (15.0, 0, 0, 0),
            (15.0, 17, 1, 17),  // 15+17=32 (上限) で頭打ち
            (15.0, -7, -1, -7), // 15-7=8 (下限) で底打ち
            (8.0, 0, -1, 0),    // 基準が下限なら縮小できない
        ];
        for (base, steps, dir, want) in table {
            assert_eq!(
                file_step(base, steps, dir),
                want,
                "file_step({base}, {steps}, {dir})"
            );
        }
    }

    #[test]
    fn file_step_survives_broken_base() {
        // 基準が壊れていても panic せず、段数は有限のまま
        let s = file_step(f32::NAN, 0, 1);
        assert!(s.abs() <= 64);
    }

    #[test]
    fn file_label_hides_the_neutral_state() {
        assert_eq!(file_label(0), None);
        assert_eq!(file_label(2).as_deref(), Some("+2pt"));
        assert_eq!(file_label(-1).as_deref(), Some("-1pt"));
    }

    // ---- wheel_steps ----

    #[test]
    fn wheel_steps_needs_a_real_gesture() {
        let mut a = 0.0;
        assert_eq!(wheel_steps(&mut a, 1.0), 0, "動きが無ければ 0 段");
        assert_eq!(a, 0.0);
    }

    #[test]
    fn wheel_steps_accumulates_small_gestures() {
        let mut a = 0.0;
        // 1 回では足りない小さなピンチでも、続ければいつか 1 段になる
        let mut total = 0;
        for _ in 0..4 {
            total += wheel_steps(&mut a, 1.04);
        }
        assert_eq!(total, 1, "0.04 ずつでも溜まって 1 段進む");
    }

    #[test]
    fn wheel_steps_is_symmetric() {
        let mut a = 0.0;
        let up = wheel_steps(&mut a, 1.5);
        let mut b = 0.0;
        let down = wheel_steps(&mut b, 1.0 / 1.5);
        assert_eq!(up, -down, "拡大と縮小で同じ段数");
    }

    #[test]
    fn wheel_steps_is_capped_per_frame() {
        let mut a = 0.0;
        assert!(wheel_steps(&mut a, 100.0) <= 4);
        let mut b = 0.0;
        assert!(wheel_steps(&mut b, 0.001) >= -4);
    }

    #[test]
    fn wheel_steps_resets_on_broken_input() {
        let mut a = 0.5;
        assert_eq!(wheel_steps(&mut a, f32::NAN), 0);
        assert_eq!(a, 0.0, "壊れた入力では溜めを捨てる");
    }
}
