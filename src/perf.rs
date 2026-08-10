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
/// **N 秒後に 1 回だけ**レポートを書き出す秒数。未設定なら書き出さない。
///
/// [`dump`] は普段 `on_exit` からしか呼ばれないので、**ウィンドウを手で
/// 閉じないとレポートが出ない**。SIGTERM ではハンドラが無く即終了するため、
/// 「起動 → N 秒放置 → 数字を取る」を script から回せなかった
/// (`tools/idle-cpu.sh` が実際にここで詰まった)。
pub const ENV_DUMP_AFTER: &str = "ZAIVERN_PERF_DUMP_AFTER";
/// レポート行の接頭辞 (grep / diff しやすいように固定)。
pub const TAG: &str = "ZAIVERN_PERF";
/// ヒストグラムの内訳行の接頭辞。
pub const TAG_BUCKET: &str = "ZAIVERN_PERF_BUCKET";
/// 再描画要求の出所ごとの内訳行の接頭辞。
pub const TAG_REPAINT: &str = "ZAIVERN_PERF_REPAINT";

/// 出所の種類の上限。これを超えたぶんは `other` にまとめる
/// (無制限に増やすと計測自体がメモリを食う)。
const REPAINT_TAGS_CAP: usize = 64;

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
    /// **アイドル中の再描画要求を、出所ごとに数えたもの。**
    ///
    /// `idle_fps` は「アイドルなのに何 fps 出ているか」しか教えてくれない。
    /// 原因を潰すには**誰が要求したか**が要る (仮説から入って 3 回外した
    /// 実績があるので、最初に分布を取れる形にしておく)。
    /// 実入力があったフレームは数えない — 入力に応じた再描画は正常なので、
    /// 混ぜると本命が埋もれる。
    pub repaints: std::collections::BTreeMap<&'static str, u64>,
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
            repaints: std::collections::BTreeMap::new(),
            started: Instant::now(),
        }
    }

    /// アイドル時の実描画レート (fps)。経過が 0 なら None。
    ///
    /// 実時間を読むのはここだけ。**判定に使う数値は
    /// [`idle_fps_over`](Self::idle_fps_over) 側 (純粋) で作る** —
    /// 実時間で線を引くテストは必ず嘘をつくため。
    pub fn idle_fps(&self) -> Option<f64> {
        self.idle_fps_over(self.started.elapsed().as_secs_f64())
    }

    /// 経過 `elapsed_s` 秒に対するアイドル描画レート (fps)。**純粋**。
    ///
    /// 経過が 0 以下 / 非数なら None (0 と偽らない)。
    pub fn idle_fps_over(&self, elapsed_s: f64) -> Option<f64> {
        (elapsed_s > 0.0).then(|| self.idle_frames as f64 / elapsed_s)
    }

    /// アイドル中に出た再描画要求の総数。
    ///
    /// **設計原則 3 が本当に見たいのはフレーム時間ではなくこの数**。
    /// 再描画が damage 駆動だけなら、放置している間ここは増えない。
    pub fn idle_repaints(&self) -> u64 {
        self.repaints.values().sum()
    }

    /// 経過 `elapsed_s` 秒に対するアイドル再描画要求のレート (件/秒)。**純粋**。
    pub fn idle_repaint_rate_over(&self, elapsed_s: f64) -> Option<f64> {
        (elapsed_s > 0.0).then(|| self.idle_repaints() as f64 / elapsed_s)
    }

    /// **自走している出所** — アイドルフレームと同数以上の要求を出したもの。
    ///
    /// アイドルフレームは「実入力が無いのに描いたフレーム」なので、
    /// ある出所の要求数がそれと同数以上なら、**そのフレームは自分で呼んだ**
    /// ことになる (= 次のフレームを自分で予約し続けている = 常時アニメーション)。
    /// 逆に「たまに起こす」出所 (git ジョブの完了通知など) は要求数が
    /// アイドルフレーム数よりずっと小さくなるので、ここには出ない。
    ///
    /// **絶対時間ではなく比で見る**のが肝心で、遅いマシンでも速いマシンでも
    /// 同じ結論になる (fps が落ちればアイドルフレームも要求も同じだけ減る)。
    /// アイドルフレームが 0 なら空 (犯人がいないのではなく、標本が無い)。
    pub fn always_on_sources(&self) -> Vec<(&'static str, u64)> {
        if self.idle_frames == 0 {
            return Vec::new();
        }
        self.repaint_ranking()
            .into_iter()
            .filter(|(_, n)| *n >= self.idle_frames)
            .collect()
    }

    /// 1 フレームぶんを積む。**純粋** (グローバル状態も実時間も触らない)。
    ///
    /// [`frame_end`] の中身をここへ出してあるので、アイドル N フレームを
    /// 模擬した検査がプロセスの計測状態を汚さずに書ける
    /// (テスト用のカウンタを共有 static に置くと、同時に走っている他の
    ///  テストの呼び出しまで混ざる)。
    ///
    /// `pending` はそのフレーム中に来た再描画要求。**アイドルでなければ
    /// 捨てる** — 入力に応じた再描画は正常なので、分布に混ぜると本命が埋もれる。
    pub fn record_frame(
        &mut self,
        us: u64,
        idle: bool,
        pending: impl IntoIterator<Item = (&'static str, u64)>,
    ) {
        self.frames.record(us);
        if !idle {
            return;
        }
        self.idle_frames += 1;
        for (tag, n) in pending {
            if self.repaints.len() >= REPAINT_TAGS_CAP && !self.repaints.contains_key(tag) {
                *self.repaints.entry("other").or_insert(0) += n;
            } else {
                *self.repaints.entry(tag).or_insert(0) += n;
            }
        }
    }

    /// 版間比較のための 1 行 1 レコード。**この関数は純粋** (I/O をしない)。
    pub fn report_lines(&self, version: &str) -> Vec<String> {
        let ms = |us: Option<u64>| us.map(|v| v as f64 / 1000.0).unwrap_or(0.0);
        // 経過は 1 回だけ読む (2 回読むと fps とレートが別の瞬間の値になる)。
        let elapsed_s = self.started.elapsed().as_secs_f64();
        // 犯人がいないときも鍵は残す (awk / diff で列がずれない)。
        let always_on = self.always_on_sources();
        let always_on = if always_on.is_empty() {
            "-".to_string()
        } else {
            always_on
                .iter()
                .map(|(t, _)| *t)
                .collect::<Vec<_>>()
                .join(",")
        };
        let mut out = vec![format!(
            "{TAG} version={version} frames={} elapsed_s={:.2} \
             p50_ms={:.3} p95_ms={:.3} p99_ms={:.3} max_ms={:.3} min_ms={:.3} mean_ms={:.3} \
             idle_frames={} idle_fps={:.3} idle_repaints={} idle_repaint_rate={:.3} \
             always_on={}",
            self.frames.count(),
            elapsed_s,
            ms(self.frames.quantile(0.50)),
            ms(self.frames.quantile(0.95)),
            ms(self.frames.quantile(0.99)),
            ms(self.frames.max_us()),
            ms(self.frames.min_us()),
            self.frames.mean_us().unwrap_or(0.0) / 1000.0,
            self.idle_frames,
            self.idle_fps_over(elapsed_s).unwrap_or(0.0),
            self.idle_repaints(),
            self.idle_repaint_rate_over(elapsed_s).unwrap_or(0.0),
            always_on,
        )];
        for (lo, hi, n) in self.frames.nonempty() {
            out.push(format!(
                "{TAG_BUCKET} lo_ms={:.3} hi_ms={:.3} count={n}",
                lo as f64 / 1000.0,
                hi as f64 / 1000.0
            ));
        }
        // **多い順**に出す。アイドル再描画の犯人は上から数行で分かる。
        let mut by_count: Vec<(&&str, &u64)> = self.repaints.iter().collect();
        by_count.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        for (tag, n) in by_count {
            out.push(format!("{TAG_REPAINT} source={tag} count={n}"));
        }
        out
    }

    /// アイドル中の再描画要求を多い順に返す (UI / テストから読む)。
    pub fn repaint_ranking(&self) -> Vec<(&'static str, u64)> {
        let mut v: Vec<(&'static str, u64)> = self.repaints.iter().map(|(k, v)| (*k, *v)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        v
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
/// 再描画要求の記録 ([`note_repaint`]) も、この旗が立っている間だけ数える。
static IDLE_FLAG: AtomicBool = AtomicBool::new(false);

/// [`ENV_DUMP_AFTER`] の値を [`std::time::Duration`] へ。**純粋** (環境を読まない)。
///
/// 秒を小数で受ける。0 以下・非数・空は `None` = 「予約しない」。
/// 数字でない値を黙って 0 扱いにすると、**起動直後に空のレポートが出て
/// 「アイドルでは何も起きていない」という嘘**になるので、必ず弾く。
fn parse_dump_after(raw: &str) -> Option<std::time::Duration> {
    let secs: f64 = raw.trim().parse().ok()?;
    (secs.is_finite() && secs > 0.0).then(|| std::time::Duration::from_secs_f64(secs))
}

/// [`ENV_DUMP_AFTER`] が指定されていれば、**1 回だけ**書き出しを予約する。
///
/// * 予約するのは**プロセスで 1 回**きり (`OnceLock`)。フレームごとに
///   スレッドを撒かない。
/// * このスレッドは**再描画を 1 度も要求しない**。計測のために設計原則 3 を
///   破らないため、寝て・書いて・終わるだけ。
fn arm_dump_timer() {
    static ARMED: OnceLock<()> = OnceLock::new();
    ARMED.get_or_init(|| {
        let Some(after) = std::env::var(ENV_DUMP_AFTER)
            .ok()
            .as_deref()
            .and_then(parse_dump_after)
        else {
            return;
        };
        let _ = std::thread::Builder::new()
            .name("perf-dump".into())
            .spawn(move || {
                std::thread::sleep(after);
                dump();
            });
    });
}

/// フレームの開始時刻を取る。無効なら `None` (以降の計測も全部止まる)。
#[inline]
pub fn frame_start() -> Option<Instant> {
    if !enabled() {
        return None;
    }
    arm_dump_timer();
    Some(Instant::now())
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
    // このフレームぶんの要求を取り出す。アイドルでなければ捨てる
    // (入力に応じた再描画は正常なので、分布に混ぜると本命が埋もれる)。
    let pending = frame_repaints()
        .lock()
        .map(|mut f| std::mem::take(&mut *f))
        .unwrap_or_default();
    if let Ok(mut s) = state().lock() {
        s.record_frame(us, idle, pending);
    }
}

/// **アイドル中の再描画要求を 1 件記録する。**
///
/// `tag` は要求元を表す短い固定文字列 (`"blame"` / `"toast"` / `"pty"` …)。
/// `&'static str` に限るのは、計測のために文字列を確保しないため。
///
/// 実入力があったフレームでは数えない — 入力に応じた再描画は正常で、
/// 混ぜると「アイドルなのに描き続けている本命」が埋もれる。
/// 無効時は `enabled()` の読み出し 1 回で戻る。
#[inline]
pub fn note_repaint(tag: &'static str) {
    if !enabled() {
        return;
    }
    // **要求の時点では「アイドルか」がまだ確定していない。**
    // `note_idle` はフレームの後半で呼ばれるので、ここで旗を見ると
    // それより前に来た要求 (= ほとんど全部) を数え損ねる
    // (実際に内訳が 0 行のまま出てきた)。いったん貯めて、
    // フレームの終わり (`frame_end`) にアイドルと判れば本体へ足す。
    if let Ok(mut f) = frame_repaints().lock() {
        if f.len() >= REPAINT_TAGS_CAP && !f.contains_key(tag) {
            *f.entry("other").or_insert(0) += 1;
            return;
        }
        *f.entry(tag).or_insert(0) += 1;
    }
}

/// このフレーム中に来た再描画要求 (アイドルと確定してから本体へ移す)。
fn frame_repaints() -> &'static Mutex<std::collections::BTreeMap<&'static str, u64>> {
    static F: OnceLock<Mutex<std::collections::BTreeMap<&'static str, u64>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(std::collections::BTreeMap::new()))
}

/// 再描画を要求しつつ、出所を記録する。**アプリ側はこれを通すこと。**
///
/// 素の `ctx.request_repaint()` を直に呼ぶと、アイドル時に誰が描かせて
/// いるのか永久に分からない (設計原則 3 を数値で守れなくなる)。
#[inline]
pub fn repaint(ctx: &eframe::egui::Context, tag: &'static str) {
    note_repaint(tag);
    ctx.request_repaint();
}

/// 時間指定つきの [`repaint`]。
#[inline]
pub fn repaint_after(ctx: &eframe::egui::Context, after: std::time::Duration, tag: &'static str) {
    note_repaint(tag);
    ctx.request_repaint_after(after);
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
    // アイドル再描画の**筆頭の犯人**をその場に出す。
    // 「アイドルなのに N fps 出ている」だけでは直せない — 誰が要求したかが
    // 見えて初めて手が付けられる (仮説から入って外し続けないため)。
    // **自走している出所は名指しで出す** — 「アイドルなのに N fps 出ている」
    // だけでは直せない。誰が次のフレームを予約し続けているかが見えて
    // 初めて手が付く (仮説から入って外し続けないため)。
    let always_on = s.always_on_sources();
    let top = s
        .repaint_ranking()
        .first()
        .map(|(tag, n)| {
            let mark = if always_on.iter().any(|(t, _)| t == tag) {
                " 常時"
            } else {
                ""
            };
            format!("  ← {tag} x{n}{mark}")
        })
        .unwrap_or_default();
    Some(format!(
        "{} frames  p50 {:.1}ms  p95 {:.1}ms  max {:.1}ms  idle {:.1}/s  req {}{top}",
        s.frames.count(),
        ms(s.frames.quantile(0.50)),
        ms(s.frames.quantile(0.95)),
        ms(s.frames.max_us()),
        s.idle_fps().unwrap_or(0.0),
        s.idle_repaints(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 再描画の出所 ──────────────────────────────────────────────────

    /// 多い順に並ぶ。同数はタグ名で安定させる (レポートの再現性)。
    #[test]
    fn 再描画の出所は多い順に並ぶ() {
        let mut st = FrameStats::new();
        st.repaints.insert("blame", 3);
        st.repaints.insert("toast", 10);
        st.repaints.insert("pty", 3);
        assert_eq!(
            st.repaint_ranking(),
            vec![("toast", 10), ("blame", 3), ("pty", 3)]
        );
    }

    /// レポート行に出所の内訳が載る (版間で diff できる形)。
    #[test]
    fn レポートに再描画の内訳が載る() {
        let mut st = FrameStats::new();
        st.repaints.insert("blame", 2);
        let lines = st.report_lines("test");
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with(TAG_REPAINT) && l.contains("source=blame count=2")),
            "内訳行が無い: {lines:?}"
        );
    }

    /// 出所が 1 件も無ければ内訳行は出さない (常に 0 の行を並べない)。
    #[test]
    fn 出所が無ければ内訳行は出さない() {
        let st = FrameStats::new();
        assert!(!st
            .report_lines("test")
            .iter()
            .any(|l| l.starts_with(TAG_REPAINT)));
    }

    // ── アイドルの検査 ────────────────────────────────────────────────
    //
    // **絶対時間で線を引かない。** ここで見るのは
    //   (1) 要求の**回数** (2) 構造の**大きさ** (3) 入力を 2 倍にしたときの伸び
    // だけ。実時間を読む `idle_fps` ではなく純粋な `idle_fps_over` を通す。
    //
    // 計測の本体はプロセス共通の `state()` に載るが、以下は全部
    // **ローカルの `FrameStats`** を組み立てて回す。共有 static を触ると
    // 同時に走っている他のテストの呼び出しが混ざる。

    /// テスト用に `&'static str` のタグを n 個作る。
    ///
    /// 件数は呼ぶ側で有界にしてあるので、意図的に leak させる
    /// (`&'static str` を要求する API を、確保無しで検査するため)。
    fn tags(n: usize) -> Vec<&'static str> {
        (0..n)
            .map(|i| &*Box::leak(format!("t{i}").into_boxed_str()))
            .collect()
    }

    /// **damage 駆動を守っていれば、放置しても再描画要求は 1 件も出ない。**
    /// アイドルを N フレーム模擬して、内訳が空のままであることを見る。
    #[test]
    fn アイドルを続けても再描画要求は増えない() {
        let mut s = FrameStats::new();
        for _ in 0..600 {
            s.record_frame(1_000, true, []);
        }
        assert_eq!(s.idle_frames, 600);
        assert_eq!(s.idle_repaints(), 0);
        assert!(
            s.repaint_ranking().is_empty(),
            "誰も要求していないのに内訳が出た: {:?}",
            s.repaint_ranking()
        );
        assert!(s.always_on_sources().is_empty());
        // レポートにも内訳行は出ない (常に 0 の行を並べない)
        assert!(!s
            .report_lines("test")
            .iter()
            .any(|l| l.starts_with(TAG_REPAINT)));
    }

    /// 実入力があったフレームの要求は数えない。
    /// (入力に応じた再描画は正常なので、混ぜると本命が埋もれる。)
    #[test]
    fn 入力のあったフレームの要求は数えない() {
        let mut s = FrameStats::new();
        for _ in 0..100 {
            s.record_frame(1_000, false, [("typing", 1)]);
        }
        assert_eq!(s.idle_frames, 0);
        assert_eq!(s.idle_repaints(), 0);
        assert!(s.repaint_ranking().is_empty());
    }

    /// **毎フレーム自分で次を呼んでいる出所は名指しで出る。**
    /// たまに起こすだけの出所 (git ジョブ完了など) は出ない。
    #[test]
    fn 自走している出所だけが名指しされる() {
        let mut s = FrameStats::new();
        for i in 0..300 {
            // spinner は毎フレーム、git_job_done は 1 回だけ。
            let mut pending = vec![("spinner", 1)];
            if i == 7 {
                pending.push(("git_job_done", 1));
            }
            s.record_frame(1_000, true, pending);
        }
        assert_eq!(s.idle_repaints(), 301);
        assert_eq!(
            s.repaint_ranking(),
            vec![("spinner", 300), ("git_job_done", 1)]
        );
        assert_eq!(
            s.always_on_sources(),
            vec![("spinner", 300)],
            "毎フレーム要求している出所だけが自走"
        );
        // レポートの 1 行目にも犯人が載る (grep 1 発で分かる)
        let head = &s.report_lines("test")[0];
        assert!(head.contains("always_on=spinner"), "{head}");
        assert!(head.contains("idle_repaints=301"), "{head}");
    }

    /// 犯人がいなくても鍵は残す (awk / diff で列がずれない)。
    #[test]
    fn 自走が無ければ犯人欄はハイフン() {
        let mut s = FrameStats::new();
        s.record_frame(1_000, true, []);
        assert!(s.report_lines("test")[0].contains("always_on=-"));
    }

    /// アイドルフレームが 0 のときは犯人を名指ししない (標本が無いだけ)。
    #[test]
    fn 標本が無いのに犯人を名指ししない() {
        let s = FrameStats::new();
        assert!(s.always_on_sources().is_empty());
        assert_eq!(s.idle_fps_over(1.0), Some(0.0));
    }

    /// `note_idle(true)` を N 回続けたあとのアイドル fps。
    /// **実時間ではなく渡した経過**で割る (実測で線を引くと必ず嘘をつく)。
    #[test]
    fn アイドルfpsは経過ぶんで割った値になる() {
        let mut s = FrameStats::new();
        for _ in 0..120 {
            s.record_frame(1_000, true, [("spinner", 1)]);
        }
        assert_eq!(s.idle_fps_over(2.0), Some(60.0));
        assert_eq!(s.idle_repaint_rate_over(2.0), Some(60.0));
        // 同じ標本でも経過が 2 倍なら fps は半分 (割り算が効いている)
        assert_eq!(s.idle_fps_over(4.0), Some(30.0));
        // 経過 0 / 負 / 非数では出さない (0 と偽らない)
        for bad in [0.0, -1.0, f64::NAN] {
            assert_eq!(s.idle_fps_over(bad), None, "elapsed={bad}");
            assert_eq!(s.idle_repaint_rate_over(bad), None, "elapsed={bad}");
        }
    }

    /// **記録は O(1) メモリ。** 標本を 2 倍にしても表の大きさは 1 要素も増えない。
    #[test]
    fn 記録のメモリは標本数に依らない() {
        let size = |n: usize| {
            let mut s = FrameStats::new();
            for i in 0..n {
                // 値をばらけさせて、バケットを広く使わせる
                s.record_frame((i as u64 % 50_000) + 1, true, [("spinner", 1)]);
            }
            (s.frames.buckets.len(), s.repaints.len(), s.frames.count())
        };
        let (b1, r1, c1) = size(1_000);
        let (b2, r2, c2) = size(2_000);
        assert_eq!(c2, c1 * 2, "標本は 2 倍になっている");
        assert_eq!(b1, BUCKETS);
        assert_eq!(b2, b1, "標本を 2 倍にしたらバケットが増えた (O(1) でない)");
        assert_eq!((r1, r2), (1, 1), "出所は 1 種類のまま");
    }

    /// **出所の種類も O(1)。** 際限なく増える入力でも上限 + `other` で頭打ち。
    #[test]
    fn 出所の種類は上限で頭打ちになる() {
        let all = tags(REPAINT_TAGS_CAP * 4);
        let size = |n: usize| {
            let mut s = FrameStats::new();
            for t in &all[..n] {
                s.record_frame(1_000, true, [(*t, 1)]);
            }
            (s.repaints.len(), s.idle_repaints())
        };
        let (small, _) = size(REPAINT_TAGS_CAP / 2);
        assert_eq!(small, REPAINT_TAGS_CAP / 2, "上限までは素通し");
        let (n2, sum2) = size(REPAINT_TAGS_CAP * 2);
        let (n4, sum4) = size(REPAINT_TAGS_CAP * 4);
        assert_eq!(n2, n4, "入力を 2 倍にしたら種類が増えた (O(1) でない)");
        assert!(
            n4 <= REPAINT_TAGS_CAP + 1,
            "上限 + other を超えた: {n4} > {}",
            REPAINT_TAGS_CAP + 1
        );
        // 溢れたぶんは捨てずに other へ足す (合計は入力と一致する)
        assert_eq!(sum2 as usize, REPAINT_TAGS_CAP * 2);
        assert_eq!(sum4 as usize, REPAINT_TAGS_CAP * 4);
    }

    /// **アイドルの定義は app.rs が持ち、perf は受け取るだけ** という配線を守る。
    ///
    /// `note_idle` が消えると、以降のレポートの `idle_frames` が永久に 0 に
    /// なり、「アイドルでは何も起きていない」という静かな嘘が出る
    /// (フレーム時間の数字は出続けるので気付けない)。
    ///
    /// ソースは `include_str!` ではなく実行時に読む — app.rs は 2MB 近くあり、
    /// テストバイナリへ 2 本目の複製を入れると nextest の 1 件 1 プロセス起動が
    /// そのぶん重くなる。改行は正規化する (Windows のチェックアウトは CRLF)。
    #[test]
    fn アイドル判定がappから渡されている() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("app.rs");
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} が読めない: {e}", path.display()))
            .replace("\r\n", "\n");
        let call = src
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("crate::perf::note_idle("))
            .unwrap_or_else(|| {
                panic!("app.rs が perf::note_idle を呼んでいない (idle_frames が永久に 0 になる)")
            });
        assert!(
            call.contains("had_input"),
            "アイドル判定が入力の有無を見ていない: {call}"
        );
    }

    /// 予約の秒数は「数字でない値を 0 扱いしない」。
    /// 黙って 0 にすると起動直後に空のレポートが出て、
    /// **「アイドルでは何も起きていない」という嘘**になる。
    #[test]
    fn 書き出し予約の秒数は不正な値を弾く() {
        assert_eq!(
            parse_dump_after("2.5"),
            Some(std::time::Duration::from_millis(2500))
        );
        assert_eq!(
            parse_dump_after("  30 "),
            Some(std::time::Duration::from_secs(30))
        );
        for bad in ["", "0", "-1", "abc", "1s", "NaN", "inf"] {
            assert_eq!(parse_dump_after(bad), None, "{bad:?} を受け取ってしまった");
        }
    }

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
