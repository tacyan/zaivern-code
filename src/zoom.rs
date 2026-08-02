//! ズーム倍率のはしご — 画面全体 (UI 全体) とファイル単位 (アクティブなエディタ)
//! の両方が同じ段を共有する。
//!
//! ## なぜ「段 (ladder)」なのか
//!
//! 連続値でズームすると 113% のような半端な倍率に落ち、
//!   - ステータスバーの表示が読みにくい
//!   - キーボードとホイールで到達できる倍率が食い違う
//!   - 「100% に戻したつもりが 99% だった」が起きる
//! の 3 つが同時に起きる。段に固定しておけば、どの入口 (キー / ホイール /
//! メニュー / パレット) から操作しても同じ倍率の列を行き来する。
//!
//! ## レイアウト判断は純粋関数へ
//!
//! ここは egui にも `Config` にも依存しない純粋関数だけを置く。
//! 段送りの挙動 (端での飽和・はしごから外れた値の扱い・異常値のフェイルソフト)
//! はテーブルテストで固定してあるので、UI 側は結果を信じて使える。

/// 選べる倍率の段。VS Code のズームレベルと同じく 1 段で 1 割前後変わる粒度。
///
/// **昇順・重複なしを保つこと** (`steps_are_sorted_and_unique` が守る)。
/// 1.0 を必ず含める — 「リセット = 1.0」がはしごの上に無いと、
/// リセット直後の段送りが不自然な位置へ飛ぶ。
pub const STEPS: [f32; 15] = [
    0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.25, 2.5, 2.75, 3.0,
];

/// 等倍。「リセット」はここへ戻す。
pub const DEFAULT: f32 = 1.0;

/// 最小倍率 (はしごの下端)。
pub const MIN: f32 = STEPS[0];
/// 最大倍率 (はしごの上端)。
pub const MAX: f32 = STEPS[STEPS.len() - 1];

/// 同じ倍率と見なす誤差。f32 の丸め (0.1 を足した結果など) を吸収する。
const EPS: f32 = 1e-4;

/// 倍率を有効範囲へ収める。異常値 (NaN / 無限) は等倍へフェイルソフトする。
///
/// ここで panic すると設定ファイルの 1 文字で起動不能になるので、絶対に assert しない。
pub fn clamp(z: f32) -> f32 {
    if !z.is_finite() {
        return DEFAULT;
    }
    z.clamp(MIN, MAX)
}

/// 1 段大きくする。上端では上端のまま (飽和)。
///
/// はしごに無い値 (手書き config の `ui_zoom = 1.05` など) からでも
/// 「いまより大きい最初の段」へ着地する。
pub fn step_up(cur: f32) -> f32 {
    let cur = clamp(cur);
    STEPS
        .iter()
        .copied()
        .find(|s| *s > cur + EPS)
        .unwrap_or(MAX)
}

/// 1 段小さくする。下端では下端のまま (飽和)。
pub fn step_down(cur: f32) -> f32 {
    let cur = clamp(cur);
    STEPS
        .iter()
        .rev()
        .copied()
        .find(|s| *s < cur - EPS)
        .unwrap_or(MIN)
}

/// 指定した段数だけ動かす (正=拡大 / 負=縮小 / 0=そのまま)。
pub fn step_by(cur: f32, steps: i32) -> f32 {
    let mut z = clamp(cur);
    for _ in 0..steps.abs() {
        z = if steps > 0 { step_up(z) } else { step_down(z) };
    }
    z
}

/// 表示用のラベル (`"100%"` / `"125%"`)。
pub fn label(z: f32) -> String {
    format!("{}%", (clamp(z) * 100.0).round() as i32)
}

/// 等倍か (ステータスバーのバッジを出すかの判定に使う)。
///
/// 「常に 0 を表示するバッジ」を作らないため、等倍のときは何も描かない。
pub fn is_default(z: f32) -> bool {
    (clamp(z) - DEFAULT).abs() < EPS
}

/// ホイール / ピンチの連続的な倍率変化を、はしごの段送りへ均す蓄積器。
///
/// egui は `ctx.input(|i| i.zoom_delta())` を **1 フレームあたりの乗算係数**
/// (1.0 = 変化なし) として返し、しかもスクロールを数フレームに均して滑らかにする。
/// そのまま倍率へ掛けると連続値になってしまうので、対数で貯めて
/// しきい値を超えた分だけ段を送る。
///
/// 余りは持ち越す — 捨てると、ゆっくり回したときに永遠に段が動かない。
#[derive(Default)]
pub struct WheelAccum {
    /// 貯まった `ln(係数)`。
    acc: f32,
}

/// 1 段送るのに必要な `ln(係数)`。
///
/// マウスホイールの 1 ノッチは egui 既定 (`scroll_zoom_speed = 1/200`) で
/// おおよそ `exp(0.07)` になるので、1 ノッチ = 1 段になる値を選んである。
const STEP_LN: f32 = 0.06;

impl WheelAccum {
    /// `zoom_delta()` の値を流し込み、動かすべき段数を返す。
    ///
    /// 異常値は 0 段として無視する (入力デバイス由来の NaN で倍率を壊さない)。
    pub fn feed(&mut self, zoom_delta: f32) -> i32 {
        if !zoom_delta.is_finite() || zoom_delta <= 0.0 {
            return 0;
        }
        self.acc += zoom_delta.ln();
        // ちょうど 1 段分の入力が f32 の丸めで 0.99999 段になり、段が動かないまま
        // 次フレームへ持ち越される (= ホイールを回しても反応しない瞬間がある)
        // のを防ぐ許容。単位は「段」なので 1e-3 段 = 見た目には無いに等しい。
        const TOL: f32 = 1e-3;
        let q = self.acc / STEP_LN;
        let whole = if q >= 0.0 {
            (q + TOL).floor()
        } else {
            (q - TOL).ceil()
        };
        // 暴走した入力 (一気に 100 倍など) でも段数は現実的な範囲に抑える。
        let steps = whole.clamp(-8.0, 8.0) as i32;
        if steps != 0 {
            self.acc -= steps as f32 * STEP_LN;
        }
        steps
    }

    /// 貯まりを捨てる (ズーム対象が切り替わったときなど)。
    pub fn reset(&mut self) {
        self.acc = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_are_sorted_and_unique() {
        for w in STEPS.windows(2) {
            assert!(w[0] < w[1], "昇順でない: {w:?}");
        }
        assert!(STEPS.contains(&DEFAULT), "等倍がはしごに無い");
        assert_eq!(MIN, STEPS[0]);
        assert_eq!(MAX, STEPS[STEPS.len() - 1]);
    }

    #[test]
    fn step_up_and_down_walk_the_ladder() {
        // 下端から上端まで昇り切り、同じ道を降りて戻る。
        let mut z = MIN;
        let mut seen = vec![z];
        for _ in 0..STEPS.len() * 2 {
            z = step_up(z);
            if z != *seen.last().unwrap() {
                seen.push(z);
            }
        }
        assert_eq!(seen, STEPS.to_vec(), "昇りではしごを踏み外している");

        for _ in 0..STEPS.len() * 2 {
            z = step_down(z);
        }
        assert_eq!(z, MIN, "降り切っても下端に着かない");
    }

    #[test]
    fn steps_saturate_at_both_ends() {
        assert_eq!(step_up(MAX), MAX);
        assert_eq!(step_up(MAX + 10.0), MAX);
        assert_eq!(step_down(MIN), MIN);
        assert_eq!(step_down(MIN - 10.0), MIN);
    }

    /// はしごから外れた値 (手書き config) でも、正しい向きの隣の段へ着地する。
    #[test]
    fn off_ladder_values_land_on_the_next_step() {
        for (cur, up, down) in [
            (1.05_f32, 1.1_f32, 1.0_f32),
            (1.4, 1.5, 1.25),
            (0.55, 0.6, 0.5),
            (2.9, 3.0, 2.75),
        ] {
            assert_eq!(step_up(cur), up, "step_up({cur})");
            assert_eq!(step_down(cur), down, "step_down({cur})");
        }
    }

    /// f32 の丸めで段がずれない (0.9 + 0.1 が 1.0000001 になっても 1 段だけ動く)。
    #[test]
    fn floating_point_noise_does_not_skip_a_step() {
        for s in STEPS {
            let noisy = s + 1e-6;
            assert_eq!(step_up(noisy), step_up(s), "{s} + ε");
            assert_eq!(step_down(noisy), step_down(s), "{s} + ε");
        }
    }

    #[test]
    fn clamp_fails_soft_on_bad_input() {
        assert_eq!(clamp(f32::NAN), DEFAULT);
        assert_eq!(clamp(f32::INFINITY), DEFAULT);
        assert_eq!(clamp(f32::NEG_INFINITY), DEFAULT);
        assert_eq!(clamp(0.0), MIN);
        assert_eq!(clamp(1000.0), MAX);
        assert_eq!(clamp(1.25), 1.25);
        // 異常値から段送りしても壊れない
        assert_eq!(step_up(f32::NAN), step_up(DEFAULT));
        assert_eq!(step_down(f32::NAN), step_down(DEFAULT));
    }

    #[test]
    fn step_by_moves_the_requested_number_of_steps() {
        assert_eq!(step_by(1.0, 0), 1.0);
        assert_eq!(step_by(1.0, 2), 1.25);
        assert_eq!(step_by(1.0, -2), 0.8);
        assert_eq!(step_by(1.0, 100), MAX);
        assert_eq!(step_by(1.0, -100), MIN);
    }

    #[test]
    fn label_is_whole_percent() {
        assert_eq!(label(1.0), "100%");
        assert_eq!(label(1.25), "125%");
        assert_eq!(label(0.5), "50%");
        assert_eq!(label(f32::NAN), "100%");
        // はしごの全段で「%」付きの整数になる
        for s in STEPS {
            let l = label(s);
            assert!(l.ends_with('%'), "{l}");
            assert!(l[..l.len() - 1].parse::<i32>().is_ok(), "{l}");
        }
    }

    #[test]
    fn is_default_only_at_one() {
        assert!(is_default(1.0));
        assert!(is_default(1.0 + 1e-6));
        assert!(!is_default(1.1));
        assert!(!is_default(0.9));
        assert!(is_default(f32::NAN), "異常値は等倍扱い (バッジを出さない)");
    }

    /// マウスホイール 1 ノッチ ≒ 1 段。egui の平滑化で複数フレームに割れても
    /// 合計が同じなら同じ段数になる。
    #[test]
    fn wheel_accum_one_notch_is_one_step() {
        let notch = (0.07_f32).exp();
        let mut a = WheelAccum::default();
        assert_eq!(a.feed(notch), 1);

        // 平滑化で 7 フレームに割れた場合も合計 1 段
        let mut b = WheelAccum::default();
        let part = (0.01_f32).exp();
        let total: i32 = (0..7).map(|_| b.feed(part)).sum();
        assert_eq!(total, 1);
    }

    #[test]
    fn wheel_accum_is_symmetric_and_keeps_remainder() {
        let mut a = WheelAccum::default();
        // 小さすぎる変化では段は動かない (が、貯まりは残る)
        assert_eq!(a.feed((0.03_f32).exp()), 0);
        assert_eq!(a.feed((0.03_f32).exp()), 1, "貯まりを捨てている");

        let mut b = WheelAccum::default();
        assert_eq!(b.feed((-0.07_f32).exp()), -1);

        // 貯まりを捨てられる
        let mut c = WheelAccum::default();
        c.feed((0.05_f32).exp());
        c.reset();
        assert_eq!(c.feed((0.05_f32).exp()), 0);
    }

    #[test]
    fn wheel_accum_ignores_bad_input() {
        let mut a = WheelAccum::default();
        assert_eq!(a.feed(f32::NAN), 0);
        assert_eq!(a.feed(0.0), 0);
        assert_eq!(a.feed(-1.0), 0);
        assert_eq!(a.feed(f32::INFINITY), 0);
        // 壊れた入力の後も普通に動く
        assert_eq!(a.feed((0.07_f32).exp()), 1);
    }

    #[test]
    fn wheel_accum_clamps_runaway_input() {
        let mut a = WheelAccum::default();
        let steps = a.feed(100.0);
        assert!(
            (1..=8).contains(&steps),
            "暴走入力で段数が飛びすぎ: {steps}"
        );
        // 1.0 から適用しても範囲内
        let z = step_by(1.0, steps);
        assert!((MIN..=MAX).contains(&z));
    }

    /// ホイールで動かした結果も必ずはしごの上に乗る (連続値にならない)。
    #[test]
    fn wheel_zoom_stays_on_the_ladder() {
        let mut a = WheelAccum::default();
        let mut z = DEFAULT;
        for _ in 0..40 {
            z = step_by(z, a.feed((0.07_f32).exp()));
        }
        assert!(STEPS.contains(&z), "はしごから外れた: {z}");
        assert_eq!(z, MAX);
    }
}
