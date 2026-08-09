//! フレーム時間とアイドル再描画の計測 — **既定では 1 命令も走らない**。
//!
//! ## なぜ要るか
//!
//! 設計原則 3 は「アイドル時のコストはゼロでなければならない。アイドル時の
//! CPU/GPU 使用率は**印象ではなく数値**でリリースゲートにする」と言っている。
//! 印象で語らないためには、版をまたいで突き合わせられる数値が要る。
//!
//! ## 方針
//!
//! - **有効化は環境変数だけ**。[`ENV_ENABLE`] が `1` のときだけ状態を確保する。
//!   無効時に走るのは [`enabled`] の `OnceLock` 読み出し 1 回で、
//!   ヒストグラムも Mutex も**確保すらしない**。
//! - **1 フレームごとに文字列を吐かない**。計測自体が重くなって観測対象を
//!   歪めるため、ヒストグラムへ積むだけにして、出力は明示的な要求時
//!   ([`dump`]) にまとめて 1 回だけ行う。
//! - **版間比較ができる形で出す**。`key=value` の 1 行 1 レコードなので
//!   `diff` でも `awk` でも比較できる (Zed の `ZED_MEASUREMENTS` と同じ発想)。
//! - **常時タイマーを入れない**。この module は自分からは 1 フレームも
//!   要求しない (設計原則 3 を計測のために破らない)。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// 計測を有効にする環境変数。`1` のときだけ働く。
pub const ENV_ENABLE: &str = "ZAIVERN_PERF";
/// レポートの出力先ファイル。未設定なら stderr。
pub const ENV_OUT: &str = "ZAIVERN_PERF_OUT";
/// レポート行の接頭辞 (grep / diff しやすいように固定)。
pub const TAG: &str = "ZAIVERN_PERF";
/// ヒストグラムの内訳行の接頭辞。
pub const TAG_BUCKET: &str = "ZAIVERN_PERF_BUCKET";

// ── ヒストグラム ───────────────────────────────────────────────────────
//
// HdrHistogram と同じ「オクターブ × 8 分割」。8us 未満は幅 1us の線形、
// それ以上は 1 オクターブを 8 等分するので相対誤差は最大 12.5%。
// 1 フレーム = 数百 us〜数十 ms を見るには十分で、表は 512 要素 (4KiB) に収まる。

/// 1 オクターブあたりの分割数。
const SUB: usize = 8;
/// バケット数。`u64` の全域を覆う最小値 (`bucket_of(u64::MAX)` = 495)。
const BUCKETS: usize = 496;

/// `us` が入るバケットの添字。
fn bucket_of(us: u64) -> usize {
    if (us as usize) < SUB {
        return us as usize;
    }
    let oct = 63 - us.leading_zeros() as usize; // floor(log2(us)) >= 3
    let shift = oct - 3;
    let sub = ((us >> shift) as usize) - SUB; // 0..SUB-1
    let i = (oct - 2) * SUB + sub;
    i.min(BUCKETS - 1)
}

/// バケット `i` が受け持つ範囲 `[lo, hi)` (マイクロ秒)。
///
/// 最上位のバケットは上端が `2^64` になるので、`u128` で計算してから
/// `u64::MAX` で頭打ちにする (シフトの溢れで panic させない)。
fn bucket_range(i: usize) -> (u64, u64) {
    if i < SUB {
        return (i as u64, i as u64 + 1);
    }
    let shift = (i / SUB - 1) as u32;
    let sub = (i % SUB) as u128;
    let cap = |v: u128| v.min(u64::MAX as u128) as u64;
    (
        cap((SUB as u128 + sub) << shift),
        cap((SUB as u128 + sub + 1) << shift),
    )
}

/// 固定バケットのヒストグラム。**確保は計測が有効なときだけ**。
#[derive(Clone)]
pub struct Histogram {
    buckets: Vec<u64>,
    count: u64,
    sum_us: u128,
    max_us: u64,
    min_us: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    pub fn new() -> Self {
        Self {
            buckets: vec![0; BUCKETS],
            count: 0,
            sum_us: 0,
            max_us: 0,
            min_us: u64::MAX,
        }
    }

    /// 1 標本を積む。
    pub fn record(&mut self, us: u64) {
        self.buckets[bucket_of(us)] += 1;
        self.count += 1;
        self.sum_us = self.sum_us.saturating_add(us as u128);
        self.max_us = self.max_us.max(us);
        self.min_us = self.min_us.min(us);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// 実測の最大値 (バケット丸めを受けない)。標本が無ければ None。
    pub fn max_us(&self) -> Option<u64> {
        (self.count > 0).then_some(self.max_us)
    }

    /// 実測の最小値。標本が無ければ None。
    pub fn min_us(&self) -> Option<u64> {
        (self.count > 0).then_some(self.min_us)
    }

    /// 平均 (マイクロ秒)。標本が無ければ None。
    pub fn mean_us(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum_us as f64 / self.count as f64)
    }

    /// 分位点の**上界** (マイクロ秒)。バケット幅ぶんだけ実値より大きくなりうる
    /// ので、必ず「以下」として読むこと。標本が無ければ None。
    ///
    /// `q` は 0.0..=1.0 にクランプされる。
    pub fn quantile(&self, q: f64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let q = q.clamp(0.0, 1.0);
        // 「累積が全体の q 割に達した最初のバケット」。ceil を取るので
        // q=1.0 は必ず最後の非空バケットに落ちる。
        let target = (q * self.count as f64).ceil().max(1.0) as u64;
        let mut cum = 0u64;
        for (i, n) in self.buckets.iter().enumerate() {
            if *n == 0 {
                continue;
            }
            cum += *n;
            if cum >= target {
                let (_, hi) = bucket_range(i);
                // 上界は排他なので 1 引く。実測の最大は超えない。
                return Some(hi.saturating_sub(1).min(self.max_us));
            }
        }
        Some(self.max_us)
    }

    /// 空でないバケットを `(lo_us, hi_us, count)` で列挙する。
    pub fn nonempty(&self) -> Vec<(u64, u64, u64)> {
        self.buckets
            .iter()
            .enumerate()
            .filter(|(_, n)| **n > 0)
            .map(|(i, n)| {
                let (lo, hi) = bucket_range(i);
                (lo, hi, *n)
            })
            .collect()
    }
}

// ── 集計の状態 ─────────────────────────────────────────────────────────

/// 1 回の計測窓ぶんの集計。
#[derive(Clone)]
pub struct FrameStats {
    /// フレーム時間のヒストグラム。
    pub frames: Histogram,
    /// アイドル (実入力が無い) と判定したフレーム数。
    pub idle_frames: u64,
    /// 計測を始めてからの経過。
    pub started: Instant,
}

impl Default for FrameStats {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameStats {
    pub fn new() -> Self {
        Self {
            frames: Histogram::new(),
            idle_frames: 0,
            started: Instant::now(),
        }
    }

    /// アイドル時の実描画レート (fps)。経過が 0 なら None。
    pub fn idle_fps(&self) -> Option<f64> {
        let s = self.started.elapsed().as_secs_f64();
        (s > 0.0).then(|| self.idle_frames as f64 / s)
    }

    /// 版間比較のための 1 行 1 レコード。**この関数は純粋** (I/O をしない)。
    pub fn report_lines(&self, version: &str) -> Vec<String> {
        let ms = |us: Option<u64>| us.map(|v| v as f64 / 1000.0).unwrap_or(0.0);
        let mut out = vec![format!(
            "{TAG} version={version} frames={} elapsed_s={:.2} \
             p50_ms={:.3} p95_ms={:.3} p99_ms={:.3} max_ms={:.3} min_ms={:.3} mean_ms={:.3} \
             idle_frames={} idle_fps={:.3}",
            self.frames.count(),
            self.started.elapsed().as_secs_f64(),
            ms(self.frames.quantile(0.50)),
            ms(self.frames.quantile(0.95)),
            ms(self.frames.quantile(0.99)),
            ms(self.frames.max_us()),
            ms(self.frames.min_us()),
            self.frames.mean_us().unwrap_or(0.0) / 1000.0,
            self.idle_frames,
            self.idle_fps().unwrap_or(0.0),
        )];
        for (lo, hi, n) in self.frames.nonempty() {
            out.push(format!(
                "{TAG_BUCKET} lo_ms={:.3} hi_ms={:.3} count={n}",
                lo as f64 / 1000.0,
                hi as f64 / 1000.0
            ));
        }
        out
    }
}

/// 計測が有効か。環境変数は**プロセスで 1 回だけ**読む。
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var(ENV_ENABLE).is_ok_and(|v| v == "1"))
}

/// 集計の置き場。**有効なときだけ確保する**。
fn state() -> &'static Mutex<FrameStats> {
    static S: OnceLock<Mutex<FrameStats>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(FrameStats::new()))
}

/// このフレームがアイドル (実入力なし) だったか。
/// `note_idle` が置き、`frame_end` が読んで消費する。
static IDLE_FLAG: AtomicBool = AtomicBool::new(false);

/// フレームの開始時刻を取る。無効なら `None` (以降の計測も全部止まる)。
#[inline]
pub fn frame_start() -> Option<Instant> {
    enabled().then(Instant::now)
}

/// このフレームがアイドルだったことを記録する (描画の途中で呼ぶ)。
#[inline]
pub fn note_idle(idle: bool) {
    if enabled() {
        IDLE_FLAG.store(idle, Ordering::Relaxed);
    }
}

/// フレームの終わり。`frame_start` の戻りをそのまま渡す。
#[inline]
pub fn frame_end(started: Option<Instant>) {
    let Some(t0) = started else { return };
    let us = t0.elapsed().as_micros().min(u64::MAX as u128) as u64;
    let idle = IDLE_FLAG.swap(false, Ordering::Relaxed);
    if let Ok(mut s) = state().lock() {
        s.frames.record(us);
        if idle {
            s.idle_frames += 1;
        }
    }
}

/// いまの集計の複製 (UI / テストから読む)。無効なら None。
pub fn snapshot() -> Option<FrameStats> {
    if !enabled() {
        return None;
    }
    state().lock().ok().map(|s| s.clone())
}

/// 集計を捨てて計測をやり直す (版間比較の区間を切るため)。
pub fn reset() {
    if !enabled() {
        return;
    }
    if let Ok(mut s) = state().lock() {
        *s = FrameStats::new();
    }
}

/// レポートを書き出す。`out` が `None` なら stderr。
///
/// 書けた行数を返す。**この module で I/O をするのはここだけ**なので、
/// 出力先の分岐はここをテストすれば全部見られる。
/// ファイルへは**追記**する — 版を切り替えながら同じファイルへ溜めて
/// `diff` / `awk` で比較できるようにするため。
pub fn write_report(lines: &[String], out: Option<&Path>) -> usize {
    if lines.is_empty() {
        return 0;
    }
    let body = lines.join("\n");
    let wrote = out.map(|path| {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| writeln!(f, "{body}"))
            .is_ok()
    });
    // 出力先が無い / 書けなかったときは stderr へ落とす (黙って捨てない)
    if wrote != Some(true) {
        eprintln!("{body}");
    }
    lines.len()
}

/// レポートを [`ENV_OUT`] のファイル (未設定なら stderr) へ書く。
///
/// 書けた行数を返す。計測が無効なら 0。
pub fn dump() -> usize {
    let Some(s) = snapshot() else { return 0 };
    let lines = s.report_lines(env!("CARGO_PKG_VERSION"));
    let path = std::env::var(ENV_OUT).ok().filter(|p| !p.trim().is_empty());
    write_report(&lines, path.as_deref().map(Path::new))
}

/// 画面に出す 1 行の要約。計測が無効・標本ゼロなら None (1px も出さない)。
pub fn status_line() -> Option<String> {
    let s = snapshot()?;
    if s.frames.count() == 0 {
        return None;
    }
    let ms = |us: Option<u64>| us.map(|v| v as f64 / 1000.0).unwrap_or(0.0);
    Some(format!(
        "{} frames  p50 {:.1}ms  p95 {:.1}ms  max {:.1}ms  idle {:.1}/s",
        s.frames.count(),
        ms(s.frames.quantile(0.50)),
        ms(s.frames.quantile(0.95)),
        ms(s.frames.max_us()),
        s.idle_fps().unwrap_or(0.0),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── バケット ──────────────────────────────────────────────────────

    /// バケットは隙間なく連続し、値は必ず自分のバケットの範囲に入る。
    #[test]
    fn バケットは連続していて値を取りこぼさない() {
        let mut prev_hi = 0;
        for i in 0..BUCKETS {
            let (lo, hi) = bucket_range(i);
            assert_eq!(lo, prev_hi, "バケット {i} が前と繋がっていない");
            assert!(hi > lo, "バケット {i} が空区間");
            prev_hi = hi;
        }
        for us in [0u64, 1, 7, 8, 9, 15, 16, 17, 1000, 16_667, 1_000_000] {
            let i = bucket_of(us);
            let (lo, hi) = bucket_range(i);
            assert!(lo <= us && us < hi, "{us} が bucket {i} [{lo},{hi}) に無い");
        }
    }

    /// 極端に大きい値でも添字が溢れない。
    #[test]
    fn 巨大な値でも添字が溢れない() {
        assert!(bucket_of(u64::MAX) < BUCKETS);
        let mut h = Histogram::new();
        h.record(u64::MAX);
        assert_eq!(h.count(), 1);
        assert_eq!(h.max_us(), Some(u64::MAX));
    }

    // ── ヒストグラム ──────────────────────────────────────────────────

    /// 0 標本では分位点も最大も None (0 と偽らない)。
    #[test]
    fn 標本ゼロでは分位点を出さない() {
        let h = Histogram::new();
        assert_eq!(h.count(), 0);
        assert_eq!(h.quantile(0.5), None);
        assert_eq!(h.quantile(0.99), None);
        assert_eq!(h.max_us(), None);
        assert_eq!(h.min_us(), None);
        assert_eq!(h.mean_us(), None);
        assert!(h.nonempty().is_empty());
    }

    /// 1 標本なら全ての分位点がその値 (上界として) になる。
    #[test]
    fn 標本ひとつなら全分位点が同じ() {
        let mut h = Histogram::new();
        h.record(1234);
        for q in [0.0, 0.5, 0.95, 0.99, 1.0] {
            let v = h.quantile(q).unwrap();
            assert!(
                (1234..=1234).contains(&v),
                "q={q} で {v} (実測の最大で頭打ちになるはず)"
            );
        }
        assert_eq!(h.max_us(), Some(1234));
        assert_eq!(h.mean_us(), Some(1234.0));
    }

    /// 全部同じ値なら、分位点はその値ちょうど (max で頭打ちになる)。
    #[test]
    fn 全部同じ値なら分位点もその値() {
        let mut h = Histogram::new();
        for _ in 0..1000 {
            h.record(5_000);
        }
        for q in [0.5, 0.95, 0.99, 1.0] {
            assert_eq!(h.quantile(q), Some(5_000), "q={q}");
        }
        assert_eq!(h.count(), 1000);
    }

    /// p50 / p95 / p99 の境界。1..=100 を 1 個ずつ積む。
    #[test]
    fn 分位点の境界が動かない() {
        let mut h = Histogram::new();
        for v in 1..=100u64 {
            h.record(v);
        }
        let p50 = h.quantile(0.50).unwrap();
        let p95 = h.quantile(0.95).unwrap();
        let p99 = h.quantile(0.99).unwrap();
        // バケット幅ぶん上へずれるが、真値より下がることは無い。
        assert!((50..=56).contains(&p50), "p50={p50}");
        assert!((95..=100).contains(&p95), "p95={p95}");
        assert!((99..=100).contains(&p99), "p99={p99}");
        assert!(p50 <= p95 && p95 <= p99, "単調でない");
        assert_eq!(h.max_us(), Some(100));
        assert_eq!(h.min_us(), Some(1));
    }

    /// 外れ値が 1 本混ざっても p50 は動かず、max だけが跳ねる。
    #[test]
    fn 外れ値は最大にだけ出る() {
        let mut h = Histogram::new();
        for _ in 0..999 {
            h.record(1_000);
        }
        h.record(10_000_000); // 10 秒のスパイク
        assert_eq!(h.max_us(), Some(10_000_000), "最大は実測どおり");
        // p50 / p99 はまだ 1ms 側 (1000 標本中 999 が 1ms)。
        // 分位点はバケットの**上界**なので 1000 ちょうどではなく、
        // 1000 以上・バケット幅 (約 12.5%) 以内に収まる。
        for q in [0.50, 0.99] {
            let v = h.quantile(q).unwrap();
            assert!((1_000..1_125).contains(&v), "q={q} で {v}");
        }
        // p100 だけがスパイクを拾う
        assert_eq!(h.quantile(1.0), Some(10_000_000));
    }

    /// 分位点の引数は 0..=1 にクランプされる (panic しない)。
    #[test]
    fn 分位点の引数は範囲外でも落ちない() {
        let mut h = Histogram::new();
        h.record(10);
        assert_eq!(h.quantile(-5.0), Some(10));
        assert_eq!(h.quantile(42.0), Some(10));
        assert_eq!(h.quantile(f64::NAN), Some(10));
    }

    // ── レポート ──────────────────────────────────────────────────────

    /// レポートは版間比較のために `key=value` の 1 行 1 レコード。
    #[test]
    fn レポートは機械可読な形で出る() {
        let mut s = FrameStats::new();
        for _ in 0..10 {
            s.frames.record(2_000);
        }
        s.idle_frames = 3;
        let lines = s.report_lines("9.9.9");
        assert!(lines[0].starts_with(TAG), "{}", lines[0]);
        assert!(lines[0].contains("version=9.9.9"));
        assert!(lines[0].contains("frames=10"));
        assert!(lines[0].contains("idle_frames=3"));
        assert!(lines[0].contains("p50_ms=2.000"), "{}", lines[0]);
        // 内訳は空でないバケットだけ
        let buckets: Vec<_> = lines.iter().filter(|l| l.starts_with(TAG_BUCKET)).collect();
        assert_eq!(buckets.len(), 1, "同じ値なのでバケットは 1 本");
        assert!(buckets[0].contains("count=10"));
    }

    /// 標本ゼロでもレポートは壊れない (0 で埋まるだけ)。
    #[test]
    fn 標本ゼロでもレポートは壊れない() {
        let s = FrameStats::new();
        let lines = s.report_lines("0.0.0");
        assert_eq!(lines.len(), 1, "内訳行は出ない");
        assert!(lines[0].contains("frames=0"));
        assert!(lines[0].contains("p50_ms=0.000"));
    }

    /// 出力先ファイルへは**追記**する (版を切り替えて溜められる)。
    #[test]
    fn レポートはファイルへ追記される() {
        let dir = crate::test_util::unique_temp_dir("zv-perf", "out");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("perf.txt");
        let a = vec![format!("{TAG} version=1 frames=1")];
        let b = vec![format!("{TAG} version=2 frames=2")];
        assert_eq!(write_report(&a, Some(&path)), 1);
        assert_eq!(write_report(&b, Some(&path)), 1);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("version=1"), "{body}");
        assert!(body.contains("version=2"), "追記されていない: {body}");
        assert_eq!(body.lines().count(), 2);
        // 空なら何も書かない (空ファイルを作らない)
        let empty = dir.join("empty.txt");
        assert_eq!(write_report(&[], Some(&empty)), 0);
        assert!(!empty.exists());
        // 書けない先 (ディレクトリを指す) でも落ちず、行数は返る
        assert_eq!(write_report(&a, Some(&dir)), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 無効時は状態を一切触らない (Mutex も確保しない)。
    /// テストプロセスでは `ZAIVERN_PERF` を立てないので必ずこの経路。
    #[test]
    fn 無効時は計測経路が走らない() {
        assert!(!enabled(), "テストでは計測は無効のはず");
        assert!(frame_start().is_none());
        frame_end(None); // 何も起きない
        note_idle(true);
        assert!(snapshot().is_none());
        assert!(status_line().is_none());
        assert_eq!(dump(), 0);
        reset();
    }
}
