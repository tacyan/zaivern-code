use std::path::Path;

use eframe::egui::{text::LayoutJob, Color32, FontId, TextFormat};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Highlighter as SynHighlighter, Theme, ThemeSet};
use syntect::highlighting::{HighlightIterator, HighlightState, Highlighter as ThemeHighlighter};
use syntect::parsing::{ParseState, Scope, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::grammar::{self, FoldKindSpec, Grammar, GrammarSet, ScanState, Span, Tok};

/// **1 回の呼び出しで全文を舐めてよい**上限。
///
/// これを超える文書は「諦めて素の文字にする」のではなく、
/// **可視域だけをチェックポイントから再開して塗る**経路
/// ([`Highlighter::layout_job_visible`]) へ落ちる。
/// 全文を 1 度に塗ると、実測 (src/app.rs = 2.0MB / 43,887 行) で
/// syntect だけで秒単位かかりフレームが止まるため。
const MAX_HIGHLIGHT_BYTES: usize = 400_000;

/// Single lines longer than this (e.g. minified JS) are laid out without
/// highlighting so one huge line cannot freeze the UI.
///
/// syntect の 1 行あたりの費用は行長にほぼ比例し、実測 (release) で
/// **約 19µs/KB**。8KB でおよそ 0.15ms なので、1 行が極端に長くても
/// フレーム予算を食い潰さない。ここを超えた行は色を諦めて素通しする
/// (= 色が消えるだけで、固まらない)。
const MAX_HIGHLIGHT_LINE_BYTES: usize = 8_192;

/// 行を跨ぐ解析状態のスナップショットを取る間隔 (行)。
///
/// 1 打鍵ぶんの再解析は「変更行の直前のスナップショット」から始めて
/// 「変更後に前回と状態が一致するスナップショット」で打ち切るので、
/// 再解析する行数はおおよそ **2 × この値**で頭打ちになる。
/// 小さくするほど再解析は速くなるがスナップショットの複製費用が増える。
const CHECKPOINT_LINES: usize = 256;

/// [`Highlighter::cache`] に置く `LayoutJob` の 1 件あたり上限。
///
/// 巨大な文書は行単位のキャッシュ ([`DocCache`]) 側で差分再計算するので、
/// 完成品の `LayoutJob` まで 32 件抱えるとメモリだけを食う
/// (2MB の文書なら本文だけで 64MB)。小さい文書に絞って持つ。
const JOB_CACHE_MAX_BYTES: usize = 64 * 1024;

/// 可視域を丸める単位 (行)。[`snap_window`] がこの倍数へ外側に広げる。
///
/// 呼び出し側の galley キャッシュキーはこの単位でしか動かないので、
/// **この行数ぶんスクロールするまで galley を組み直さない**。
///
/// 実測 (release, src/app.rs = 2.0MB / 43,887 行) で、`LayoutJob` から
/// galley を組む費用は **495ms**。しかも**セクション数にはほとんど依存しない**
/// (1 セクション 495ms / 213,110 セクション 509ms = +2.9%)。
/// つまり「可視域だけ塗る」で節約できるのは syntect の側だけで、
/// **可視域が動くたびに galley を組み直すと 0.5 秒ずつ持っていかれる**。
/// だから可視域は粗い粒度へ丸める。1 画面 50 行として、この値なら
/// 10 画面ぶんスクロールするまで組み直しが起きない。
const WINDOW_BLOCK_LINES: usize = 512;

/// 1 回の呼び出しで**フロンティア (先頭から連続して解析済みの位置) を
/// 進めてよい**行数の上限。
///
/// 実測 (release, src/app.rs) で `ParseState::parse_line` は **1 行 ≒ 100µs**
/// (20,060 行で 2.03 秒)。フレーム予算 16.7ms の 4 割弱に収まる行数にした。
/// ここで止めた続きは次のフレームで再開するので、遠い行へ飛んでも
/// 1 フレームに払う費用は一定に保たれる。
const WINDOW_SCAN_BUDGET_LINES: usize = 64;

/// スナップショット 1 つぶんのメモリ見積り。
///
/// `ParseState` の文脈スタックと `HighlightState` のスタイルスタックは
/// どちらも数十要素の `Vec` なので実際はもっと小さいが、言語によっては
/// 深くなるため多めに見込む (下限ではなく上限として使う)。
const CHECKPOINT_APPROX_BYTES: usize = 8 * 1024;

/// スナップショット全体で使ってよいメモリ。
const CHECKPOINT_MEM_BUDGET: usize = 4 * 1024 * 1024;

/// 抱えるスナップショットの最大数。超えたら 1 つおきに間引き、
/// 間隔 (`stride`) を倍にする — **等間隔を保つ**のが肝で、近い所だけを
/// 残すと遠い行が毎回先頭からの再計算になる。
const MAX_CHECKPOINTS: usize = CHECKPOINT_MEM_BUDGET / CHECKPOINT_APPROX_BYTES;

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// プロセスで 1 つだけの [`Highlighter`]。
///
/// `SyntaxSet` / `ThemeSet` は数 MB あるので、エディタ本文・差分ビュー・
/// Markdown プレビューがそれぞれ持つとメモリが素直に倍々になる。
/// 内部のキャッシュは `Mutex` 越しなので複数箇所から同時に呼んで安全。
/// **初回呼び出しまでロードしない**ので、起動時間には影響しない。
pub fn shared() -> &'static Highlighter {
    static H: OnceLock<Highlighter> = OnceLock::new();
    H.get_or_init(Highlighter::new)
}

/// 行内の 1 区間。差分再計算のために「行ごとの塗り分け」を持ち回る単位。
#[derive(Clone, Copy, PartialEq, Debug)]
struct LineSpan {
    /// **行頭からの**バイト範囲 (半開区間)。
    start: u32,
    end: u32,
    color: Color32,
    italic: bool,
    underline: bool,
}

/// 行を跨ぐ解析状態のスナップショット。`line` 行目を**読む直前**の状態。
#[derive(Clone)]
struct Checkpoint {
    line: usize,
    ps: ParseState,
    hs: HighlightState,
}

/// 直前に塗った文書の記憶。1 文書ぶんだけ持つ。
///
/// 何万行のファイルで 1 文字打つたびに全行を解析し直すと、実測で秒単位
/// かかりフレーム予算 (16ms) を軽く割る。ここに「行ごとの塗り分け」と
/// 「一定間隔の解析状態」を残しておき、変更点の前後だけを解析し直す。
struct DocCache {
    /// 本文以外のキー (言語・テーマ・フォント・既定色)。変わったら作り直す。
    style_key: u64,
    /// 直前の本文を行に割ったもの (改行込み)。
    lines: Vec<String>,
    /// 行ごとの塗り分け。`lines` と同じ長さ。
    spans: Vec<Vec<LineSpan>>,
    /// 行番号の昇順。先頭は必ず 0 行目。
    checkpoints: Vec<Checkpoint>,
}

// 直前に塗った文書を置く場所。
//
// `Highlighter` のフィールドにできない: syntect の `ParseState` は
// onig の生ポインタを抱えていて `Send` ではないため、`Mutex` に入れると
// `shared()` (= `&'static`, `Sync` 必須) が成立しなくなる。
// ハイライトを走らせるのは描画スレッドなので、スレッドローカルで足りる。
thread_local! {
    static DOC_CACHE: std::cell::RefCell<Option<DocCache>> = const { std::cell::RefCell::new(None) };
}

/// 行キャッシュを残す下限。これ未満の文書は作り直しても一瞬なので、
/// 抱えるとむしろ本命 (編集中の巨大ファイル) を押し出してしまう。
/// Markdown プレビューのコードフェンスがこれに当たる。
const DOC_CACHE_MIN_BYTES: usize = 8 * 1024;

/// 巨大文書の 1 つのチェックポイント。`cp.line` 行目を**読む直前**の状態と、
/// その位置までの本文の走行ハッシュを持つ。
///
/// ハッシュを持つのは「本文が変わっていないか」を**行を持たずに**確かめる
/// ため。前回の本文を丸ごと抱えると 2MB の文書で 2MB 余計に食ううえ、
/// 打鍵のたびに 43,887 行の比較が要る。ここでは先頭から 1 パス回して
/// **最初に食い違ったチェックポイントで切る**だけで済む。
#[derive(Clone)]
struct WinPoint {
    cp: Checkpoint,
    /// `cp.line` 行目の先頭の本文バイト位置。
    byte: usize,
    /// `text[..byte]` を流し込んだハッシャ。`finish()` で照合する
    /// (`Hasher::write` は連続入力なので、区切り方が違っても同じ値になる)。
    hasher: std::collections::hash_map::DefaultHasher,
}

/// 巨大文書の解析フロンティアとチェックポイント台帳。1 文書ぶんだけ持つ。
struct WinCache {
    /// 本文以外のキー (言語・テーマ・既定色)。変わったら作り直す。
    style_key: u64,
    /// 0 行目から `stride` 行おきのスナップショット。行番号の昇順で、
    /// **先頭は必ず 0 行目**。
    points: Vec<WinPoint>,
    /// 先頭から連続して解析し終えた位置。`points` の最後より先に居てよい
    /// (予算切れで止まった続きを捨てないため)。
    frontier: WinPoint,
    /// いまのスナップショット間隔 (行)。
    stride: usize,
    /// 直近に見た本文の行数。可視域が末尾を越えているときに
    /// 「届かない目標」を追いかけないための上限として使う。
    line_count: usize,
}

// 巨大文書のチェックポイント台帳を置く場所。
//
// `DOC_CACHE` と同じ理由でスレッドローカル (`ParseState` は `Send` でない)。
thread_local! {
    static WIN_CACHE: std::cell::RefCell<Option<WinCache>> =
        const { std::cell::RefCell::new(None) };
}

impl WinCache {
    /// 0 行目のスナップショットだけを持つ台帳を作る。
    fn new(style_key: u64, syntax: &SyntaxReference, hl: &ThemeHighlighter) -> Self {
        let zero = WinPoint {
            cp: Checkpoint {
                line: 0,
                ps: ParseState::new(syntax),
                hs: HighlightState::new(hl, syntect::parsing::ScopeStack::new()),
            },
            byte: 0,
            hasher: std::collections::hash_map::DefaultHasher::new(),
        };
        Self {
            style_key,
            points: vec![zero.clone()],
            frontier: zero,
            stride: CHECKPOINT_LINES,
            line_count: 0,
        }
    }

    /// 本文が変わっていないところまで台帳を切り詰める。
    ///
    /// 先頭から 1 パスでハッシュを取り直し、**最初に食い違った位置から先**を
    /// 捨てる。0 行目は必ず生き残る (空のハッシュ同士なので必ず一致する)。
    fn trim_to_unchanged(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let mut at = 0usize;
        let mut ok = 0usize;
        for p in &self.points {
            if p.byte > bytes.len() {
                break;
            }
            h.write(&bytes[at..p.byte]);
            at = p.byte;
            if h.finish() != p.hasher.finish() {
                break;
            }
            ok += 1;
        }
        if ok < self.points.len() {
            self.points.truncate(ok.max(1));
            // 途中で切った以上、フロンティアも生き残った末尾まで戻す。
            self.frontier = self.points[self.points.len() - 1].clone();
            return;
        }
        // 全チェックポイントが生きていた。フロンティアはその先にあるので
        // 続きだけを流して確かめる。
        if self.frontier.byte > bytes.len() || self.frontier.byte < at {
            self.frontier = self.points[self.points.len() - 1].clone();
            return;
        }
        h.write(&bytes[at..self.frontier.byte]);
        if h.finish() != self.frontier.hasher.finish() {
            self.frontier = self.points[self.points.len() - 1].clone();
        }
    }

    /// スナップショットが増えすぎたら 1 つおきに間引く (間隔は倍になる)。
    fn thin(&mut self) {
        while self.points.len() > MAX_CHECKPOINTS {
            let mut keep = Vec::with_capacity(self.points.len().div_ceil(2));
            for (i, p) in self.points.drain(..).enumerate() {
                if i.is_multiple_of(2) {
                    keep.push(p);
                }
            }
            self.points = keep;
            self.stride *= 2;
        }
    }
}

/// フロンティアを `target` 行まで進める。1 回で進めるのは `budget` 行まで。
///
/// 進めながら `stride` 行ごとにスナップショットを取る。返り値は実際に
/// syntect へ通した行数 (= 費用の実測値。テストがここを見る)。
fn win_advance(
    ps: &SyntaxSet,
    syntax: &SyntaxReference,
    hl: &ThemeHighlighter,
    lines: &[&str],
    wc: &mut WinCache,
    target: usize,
    budget: usize,
) -> usize {
    if wc.frontier.cp.line >= target {
        return 0;
    }
    // 解析状態は 1 度だけ取り出して回す (1 行ごとに複製すると、進めるより
    // 複製のほうが高くつく)。抜き取った跡には作り直した空の状態を置く。
    let mut state = (
        std::mem::replace(&mut wc.frontier.cp.ps, ParseState::new(syntax)),
        std::mem::replace(
            &mut wc.frontier.cp.hs,
            HighlightState::new(hl, syntect::parsing::ScopeStack::new()),
        ),
    );
    let mut scanned = 0usize;
    while wc.frontier.cp.line < target && scanned < budget {
        let i = wc.frontier.cp.line;
        let Some(line) = lines.get(i) else { break };
        // 色はここでは要らない (可視域に入ってから塗る) ので捨てる。
        paint_line(ps, syntax, hl, line, &mut state, Color32::WHITE);
        wc.frontier.hasher.write(line.as_bytes());
        wc.frontier.byte += line.len();
        wc.frontier.cp.line = i + 1;
        scanned += 1;
        if wc.frontier.cp.line.is_multiple_of(wc.stride) {
            wc.points.push(WinPoint {
                cp: Checkpoint {
                    line: wc.frontier.cp.line,
                    ps: state.0.clone(),
                    hs: state.1.clone(),
                },
                byte: wc.frontier.byte,
                hasher: wc.frontier.hasher.clone(),
            });
            wc.thin();
        }
    }
    wc.frontier.cp.ps = state.0;
    wc.frontier.cp.hs = state.1;
    scanned
}

/// 1 行ぶん塗って状態を進める。
///
/// **[`highlight_doc`] の 1 行ぶんと同じ規則**にしてある (長すぎる行は
/// 素通し / 解析エラーなら状態を作り直して素通し)。ここがずれると
/// 「チェックポイントから再開した結果」と「先頭から通した結果」が
/// 食い違い、文字列やコメントの色が延々と尾を引く。
fn paint_line(
    ps: &SyntaxSet,
    syntax: &SyntaxReference,
    hl: &ThemeHighlighter,
    line: &str,
    state: &mut (ParseState, HighlightState),
    fallback: Color32,
) -> Vec<LineSpan> {
    if line.len() > MAX_HIGHLIGHT_LINE_BYTES {
        return plain_span(line.len(), fallback);
    }
    match state.0.parse_line(line, ps) {
        Ok(ops) => {
            let mut v = Vec::new();
            let mut off = 0usize;
            for (style, piece) in HighlightIterator::new(&mut state.1, &ops, line, hl) {
                let end = off + piece.len();
                if end > off {
                    v.push(span_of(&style, off, end));
                }
                off = end;
            }
            if off < line.len() {
                v.push(LineSpan {
                    start: off as u32,
                    end: line.len() as u32,
                    color: fallback,
                    italic: false,
                    underline: false,
                });
            }
            v
        }
        Err(_) => {
            *state = (
                ParseState::new(syntax),
                HighlightState::new(hl, syntect::parsing::ScopeStack::new()),
            );
            plain_span(line.len(), fallback)
        }
    }
}

/// 塗る対象の行域 (半開区間)。[`snap_window`] で作る。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// 最初の行 (0 始まり)。
    pub start: usize,
    /// 最後の行の次。
    pub end: usize,
}

/// 可視域を [`WINDOW_BLOCK_LINES`] の倍数へ**外側に**広げる。
///
/// 呼び出し側は galley キャッシュのキーにも `start` / `end` を混ぜること。
/// 生のスクロール位置を混ぜると 1 行動くだけで galley を組み直すことになり、
/// 巨大ファイルではそれ自体がフレーム落ちの原因になる (実測 495ms/回)。
pub fn snap_window(first_line: usize, line_count: usize) -> Window {
    Window {
        start: first_line - first_line % WINDOW_BLOCK_LINES,
        end: (first_line + line_count.max(1)).next_multiple_of(WINDOW_BLOCK_LINES),
    }
}

/// [`Highlighter::layout_job_visible`] の結果。
pub struct VisibleJob {
    /// 本文まるごとの `LayoutJob`。可視域は塗り分け、その外は 1 セクション。
    pub job: LayoutJob,
    /// 可視域を**正しい文脈**で塗れたか。
    ///
    /// `false` は暫定表示 (可視域の先頭から解析し直した近似) を意味する。
    /// 呼び出し側は galley を捨てて次のフレームでもう一度呼ぶこと
    /// (`ctx.request_repaint()`)。何度か呼べば必ず `true` になり、
    /// **そこからは再描画を要求しない** = アイドルの費用はゼロに戻る。
    pub exact: bool,
    /// この呼び出しで syntect へ通した行数。文書全体ではなく
    /// 「チェックポイント間隔 + 可視域」で頭打ちになることを
    /// テストがここで確かめる。
    pub scanned_lines: usize,
}

/// [`Highlighter::advance_to_visible`] の結果。
pub struct Advance {
    /// 可視域を正しい文脈で塗れる所までフロンティアが届いたか。
    pub ready: bool,
    /// この呼び出しで syntect へ通した行数
    /// ([`WINDOW_SCAN_BUDGET_LINES`] で頭打ちになる)。
    pub scanned_lines: usize,
}

/// `Style` を [`LineSpan`] へ落とす (色と字体だけ残す)。
fn span_of(style: &syntect::highlighting::Style, start: usize, end: usize) -> LineSpan {
    let fg = style.foreground;
    LineSpan {
        start: start as u32,
        end: end as u32,
        color: Color32::from_rgb(fg.r, fg.g, fg.b),
        italic: style.font_style.contains(FontStyle::ITALIC),
        underline: style.font_style.contains(FontStyle::UNDERLINE),
    }
}

/// 行全体を 1 色で塗る区間 (長すぎる行・解析エラー時のフォールバック)。
fn plain_span(len: usize, color: Color32) -> Vec<LineSpan> {
    if len == 0 {
        return Vec::new();
    }
    vec![LineSpan {
        start: 0,
        end: len as u32,
        color,
        italic: false,
        underline: false,
    }]
}

/// 1 文書ぶんの塗り分けを作る。**直前の結果があれば差分だけ計算する**。
///
/// 手順は 3 つ。
/// 1. 前回と一致する**先頭**の行数を数え、その直前のスナップショットから再開する。
/// 2. 変更点から前へ進みながら解析する。
/// 3. 未変更の**末尾**に入ったあと、前回のスナップショットと解析状態が
///    一致した時点で打ち切り、そこから先は前回の結果をそのまま使う。
///
/// 3 が効くので、行数が増減しても再解析は変更点の周辺だけで済む
/// (スナップショットは行番号をずらして引き継ぐ)。
fn highlight_doc(
    ps: &SyntaxSet,
    syntax: &SyntaxReference,
    theme: &Theme,
    text: &str,
    fallback: Color32,
    style_key: u64,
    prev: Option<&DocCache>,
) -> DocCache {
    let lines: Vec<&str> = LinesWithEndings::from(text).collect();
    let n = lines.len();
    let hl = ThemeHighlighter::new(theme);

    let prev = prev.filter(|c| c.style_key == style_key && !c.checkpoints.is_empty());

    // --- 1. 前方一致 ---
    let mut head = 0usize;
    if let Some(c) = prev {
        while head < n && head < c.lines.len() && lines[head] == c.lines[head] {
            head += 1;
        }
    }
    // --- 2. 後方一致 (前方一致と重ならない範囲で) ---
    let mut tail = 0usize;
    if let Some(c) = prev {
        let old = c.lines.len();
        while tail < n - head && tail + head < old && lines[n - 1 - tail] == c.lines[old - 1 - tail]
        {
            tail += 1;
        }
    }

    let mut spans: Vec<Vec<LineSpan>> = Vec::with_capacity(n);
    let mut checkpoints: Vec<Checkpoint> = Vec::new();
    let mut state;
    let mut start = 0usize;

    match prev {
        // 再開できるスナップショット = head 行目以前で最も後ろのもの。
        Some(c) if head > 0 => {
            let k = c.checkpoints.partition_point(|cp| cp.line <= head) - 1;
            let cp = &c.checkpoints[k];
            start = cp.line;
            spans.extend_from_slice(&c.spans[..start]);
            checkpoints.extend_from_slice(&c.checkpoints[..=k]);
            state = (cp.ps.clone(), cp.hs.clone());
        }
        _ => {
            state = (
                ParseState::new(syntax),
                HighlightState::new(&hl, syntect::parsing::ScopeStack::new()),
            );
            checkpoints.push(Checkpoint {
                line: 0,
                ps: state.0.clone(),
                hs: state.1.clone(),
            });
        }
    }

    // 行数の増減。前回の行 `ol` は今回の行 `ol + delta` に当たる。
    let delta: isize = prev.map_or(0, |c| n as isize - c.lines.len() as isize);

    let mut i = start;
    while i < n {
        // --- 3. 未変更の末尾に入ったら、状態が一致した時点で打ち切る ---
        if tail > 0 && i >= n - tail {
            if let Some(c) = prev {
                let ol = i as isize - delta;
                if ol >= 0 {
                    if let Ok(k) = c
                        .checkpoints
                        .binary_search_by_key(&(ol as usize), |cp| cp.line)
                    {
                        if c.checkpoints[k].ps == state.0 && c.checkpoints[k].hs == state.1 {
                            spans.extend_from_slice(&c.spans[ol as usize..]);
                            checkpoints.extend(c.checkpoints[k..].iter().map(|cp| Checkpoint {
                                line: (cp.line as isize + delta) as usize,
                                ps: cp.ps.clone(),
                                hs: cp.hs.clone(),
                            }));
                            return DocCache {
                                style_key,
                                lines: lines.iter().map(|l| (*l).to_string()).collect(),
                                spans,
                                checkpoints,
                            };
                        }
                    }
                }
            }
        }
        if i > start && i.is_multiple_of(CHECKPOINT_LINES) {
            checkpoints.push(Checkpoint {
                line: i,
                ps: state.0.clone(),
                hs: state.1.clone(),
            });
        }

        let line = lines[i];
        if line.len() > MAX_HIGHLIGHT_LINE_BYTES {
            // 極端に長い 1 行 (minify 済み JS など) で UI を止めない。
            // 色は諦めるが、解析状態は進めない = 以降の行は直前の文脈を保つ。
            spans.push(plain_span(line.len(), fallback));
            i += 1;
            continue;
        }
        match state.0.parse_line(line, ps) {
            Ok(ops) => {
                let mut v = Vec::new();
                let mut off = 0usize;
                for (style, piece) in HighlightIterator::new(&mut state.1, &ops, line, &hl) {
                    let end = off + piece.len();
                    if end > off {
                        v.push(span_of(&style, off, end));
                    }
                    off = end;
                }
                if off < line.len() {
                    v.push(LineSpan {
                        start: off as u32,
                        end: line.len() as u32,
                        color: fallback,
                        italic: false,
                        underline: false,
                    });
                }
                spans.push(v);
            }
            Err(_) => {
                // 解析器の内部状態が壊れている可能性があるので作り直す。
                state = (
                    ParseState::new(syntax),
                    HighlightState::new(&hl, syntect::parsing::ScopeStack::new()),
                );
                spans.push(plain_span(line.len(), fallback));
            }
        }
        i += 1;
    }

    DocCache {
        style_key,
        lines: lines.iter().map(|l| (*l).to_string()).collect(),
        spans,
        checkpoints,
    }
}

/// 行ごとの塗り分けから `LayoutJob` を組む。
///
/// 同じ書式が続く区間はまとめる。egui の layout 費用はセクション数に
/// 比例するので、空白と記号が延々と続くコードでは効き目が大きい。
fn job_from_spans(text: &str, lines: &[&str], spans: &[Vec<LineSpan>], font: &FontId) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    job.text.push_str(text);
    append_spans(&mut job, 0, lines, spans, font);
    job
}

/// 行ごとの塗り分けを `job` の**セクションとして**積む。
///
/// `base` は `lines[0]` の先頭が本文のどこに当たるか (バイト)。可視域だけを
/// 塗る経路 ([`Highlighter::layout_job_visible`]) は本文の途中から積むので
/// ここを分けてある。呼び出し側が `job.text` を先に用意しておくこと。
fn append_spans(
    job: &mut LayoutJob,
    base: usize,
    lines: &[&str],
    spans: &[Vec<LineSpan>],
    font: &FontId,
) {
    let mut base = base;
    // (バイト範囲, 色, italic, underline) を貯めて、変わった時点で吐く。
    let mut cur: Option<(usize, usize, Color32, bool, bool)> = None;
    for (i, line) in lines.iter().enumerate() {
        for sp in spans.get(i).map(|v| v.as_slice()).unwrap_or(&[]) {
            let (s, e) = (base + sp.start as usize, base + sp.end as usize);
            match &mut cur {
                Some((_, ce, cc, ci, cu))
                    if *ce == s && *cc == sp.color && *ci == sp.italic && *cu == sp.underline =>
                {
                    *ce = e;
                }
                _ => {
                    if let Some((s0, e0, c, it, ul)) = cur.take() {
                        push_section(job, s0..e0, c, it, ul, font);
                    }
                    cur = Some((s, e, sp.color, sp.italic, sp.underline));
                }
            }
        }
        base += line.len();
    }
    if let Some((s0, e0, c, it, ul)) = cur.take() {
        push_section(job, s0..e0, c, it, ul, font);
    }
}

fn push_section(
    job: &mut LayoutJob,
    range: std::ops::Range<usize>,
    color: Color32,
    italic: bool,
    underline: bool,
    font: &FontId,
) {
    let mut format = TextFormat {
        font_id: font.clone(),
        color,
        ..Default::default()
    };
    format.italics = italic;
    if underline {
        format.underline = eframe::egui::Stroke::new(1.0_f32, color);
    }
    job.sections.push(eframe::egui::text::LayoutSection {
        leading_space: 0.0,
        byte_range: range,
        format,
    });
}

/// syntect の構文セットを組む。
///
/// syntect 同梱の `SyntaxSet::load_defaults_newlines()` は Sublime Text の
/// 既定パッケージぶんだけで、**実測 75 構文**しか無い。TypeScript / TSX /
/// Vue / Svelte / Kotlin / Zig / TOML / GraphQL / Terraform / Dart /
/// Elixir / Nix / Solidity / Dockerfile などが 1 つも入っておらず、
/// これらは自前の [`crate::grammar`] が正規表現で近似していた。
/// 近似はスコープ体系を持たないので、**同じテーマでも色が違って見える**。
///
/// two-face (bat が使っている拡張セット) は**実測 220 構文**を持ち、
/// 上記が全て本物の `.sublime-syntax` になる。定義は crate に
/// `include_bytes!` されたダンプなので、実行時にファイルを探しに行かない
/// (= インストール先やユーザー名に依存しない)。
///
/// Rust だけは [`RUST_SYNTAX_YAML`] の**別セット**で塗る ([`load_rust_syntax`])。
fn load_syntaxes() -> SyntaxSet {
    two_face::syntax::extra_newlines()
}

/// 同梱する最新の Rust 構文 (`assets/syntaxes/Rust.sublime-syntax`)。
///
/// **出典 / ライセンスはファイル冒頭のコメントに明記してある**
/// (sublimehq/Packages, permissive: copy/use/modify/sell/distribute 許諾)。
///
/// なぜ差し替えるのか — two-face が持つ Rust は同じ sublimehq/Packages の
/// **古い版**で、スコープをダンプして実測すると次の欠落があった:
///   * `async` / `await` にスコープが付かない → 素の識別子と同じ色
///   * トレイト境界の型名 (`impl<T: Display + Clone>` の `Display`) が無スコープ
///   * `.rs` 先頭のシェバンを `#` `!` `/` の**演算子列**として塗る
///
/// 上流の最新版はこの 3 つを全て直しているので、
/// 「足りないスコープだけ足す派生構文を自作する」より
/// **同系統の新しい版へ差し替える**方が壊れる余地が少ないと判断した。
///
/// `include_str!` でバイナリへ焼き込むので、実行時にファイルを探しに行かない
/// (= インストール先・ユーザー名・ロケールに依存しない)。
const RUST_SYNTAX_YAML: &str = include_str!("../assets/syntaxes/Rust.sublime-syntax");

/// [`RUST_SYNTAX_YAML`] **だけ**を収めた小さな構文セットを組む。
///
/// なぜ拡張セットへ混ぜないのか — `SyntaxSet::into_builder()` は 220 構文の
/// 遅延コンテキストを**全て**復号し、`build()` が全部を張り直して再直列化する。
/// 実測 (release) で **1 回 0.6〜0.8 秒**で、two-face をそのまま読む 2〜5ms に対して
/// 2 桁以上遅い。最初にファイルを開いた瞬間に 0.6 秒固まるのは割に合わないので、
/// **Rust を塗るときだけ引く別セット**にして、そこも初回まで作らない。
///
/// 1 ファイルの不備で起動不能にしない: 読めなければ `None` を返し、
/// 呼び出し側は拡張セットの Rust へ落ちる (少し古い塗り分けになるだけ)。
fn load_rust_syntax() -> Option<SyntaxSet> {
    let def =
        syntect::parsing::SyntaxDefinition::load_from_str(RUST_SYNTAX_YAML, true, Some("Rust"))
            .ok()?;
    let mut b = syntect::parsing::SyntaxSetBuilder::new();
    // `frontmatter` コンテキストが `embed: scope:source.toml` を参照する。
    // TOML はこの小セットに居ないので、syntect の「解決できない embed は
    // Plain Text へ落とす」経路 (`with_plain_text_fallback`) に乗せるため、
    // Plain Text を**必ず**入れておく (入れないと参照が未解決のまま残る)。
    b.add_plain_text_syntax();
    b.add(def);
    let set = b.build();
    // 名前で引けないセットは使わない (= 拡張セットのままにする)。
    if set.find_syntax_by_name("Rust").is_some() {
        Some(set)
    } else {
        None
    }
}

/// 端末専用テーマ。色の代わりに **ANSI パレット番号**を詰めた特殊な
/// テーマで、そのまま RGB として読むとほぼ黒になる。GUI では選ばせない。
const TERMINAL_ONLY_THEMES: &[&str] = &["ansi", "base16", "base16-256"];

/// syntect が「色を付けない」ときに使う構文名。判定の途中でこれを返すと
/// より具体的な候補を潰してしまうので、段階ごとに弾く。
const PLAIN_TEXT: &str = "Plain Text";

/// syntect のテーマ集合を組む。
///
/// 既定の 7 テーマに two-face の追加テーマを**足すだけ**で、同名のものは
/// 既定側を残す。カラーテーマ (`theme::all()`) の `syntect_theme` は
/// 既定名を指しているので、上書きすると見た目が黙って変わってしまう。
fn load_themes() -> ThemeSet {
    let mut ts = ThemeSet::load_defaults();
    let extra: ThemeSet = two_face::theme::extra().into();
    for (name, theme) in extra.themes {
        if TERMINAL_ONLY_THEMES.contains(&name.as_str()) {
            continue;
        }
        ts.themes.entry(name).or_insert(theme);
    }
    ts
}

/// 拡張セットが取り違えている拡張子／フェンストークン。
/// ここに挙げたものだけは [`crate::grammar`] のパックを先に見る。
/// **増やすときは必ず理由を書くこと。**
const SYNTECT_MISASSIGNED: &[(&str, &str)] = &[
    // 拡張セットは .fs をフラグメントシェーダ (GLSL) に割り当てるが、
    // .fs は F# の実装ファイルというのが実態。シェーダ側は .frag/.fsh を使う。
    ("fs", "拡張セットは .fs を GLSL にするが実態は F#"),
    // 同じ理由で F# のシグネチャ／スクリプトも巻き込まれないようにする。
    ("fsi", "F# のシグネチャファイル (.fs と揃える)"),
    ("fsx", "F# のスクリプト (.fs と揃える)"),
];

/// [`SYNTECT_MISASSIGNED`] に載っているか (大文字小文字を無視)。
fn is_misassigned(ext: &str) -> bool {
    SYNTECT_MISASSIGNED
        .iter()
        .any(|(e, _)| e.eq_ignore_ascii_case(ext))
}

pub struct Highlighter {
    /// syntect の構文セット。**最初に着色するまで読み込まない**。
    /// 拡張セットのダンプは約 1MB あり、起動時に必ず払う費用にしたくない
    /// ([`shared`] の遅延化だけでは、テーマ名を引くだけの経路でも
    /// 構文セットまで道連れになる)。
    ps: OnceLock<SyntaxSet>,
    /// Rust 専用の小さな構文セット ([`load_rust_syntax`])。
    /// **Rust を実際に塗るまで組まない**。組めなかったときは `None` が入り、
    /// 以降は拡張セットの Rust をそのまま使う。
    rust_ps: OnceLock<Option<SyntaxSet>>,
    /// syntect のテーマ集合。構文セットとは**独立に**遅延ロードする。
    ts: OnceLock<ThemeSet>,
    /// プラグインが持ち込んだ軽量シンタックス定義 (`[[syntax]]`)。
    /// syntect が知らない言語 (TypeScript / Kotlin / Zig …) はこちらで塗る。
    ///
    /// [`shared`] はプロセスで 1 つの `&'static` なので、プラグインの
    /// 有効/無効が切り替わったときに差し替えられるよう内部可変にしてある。
    /// 読み側は `Arc` を 1 回複製するだけで、ロックを跨いで持ち歩かない。
    packs: RwLock<Arc<GrammarSet>>,
    /// テーマ名 → トークン種類ごとの色。テーマは滅多に変わらないので
    /// 一度作ったら使い回す (毎行スコープ解決をやり直さないため)。
    palettes: Mutex<HashMap<String, Arc<Palette>>>,
    /// key → LayoutJob と、その挿入順。キーは本文全体のハッシュを含むため
    /// 1 打鍵ごとに新しいエントリが増える。上限到達で全消しすると次の
    /// フレームに全再ハイライトのスパイクが出るので、古い方から 1 件ずつ
    /// 追い出す。文書まるごとの LayoutJob を抱えるため上限は小さめ。
    cache: Mutex<(HashMap<u64, LayoutJob>, std::collections::VecDeque<u64>)>,
}

const HL_CACHE_CAP: usize = 32;

/// トークン種類ごとの見た目。syntect のテーマスコープから引くので、
/// 追加言語の配色も**カラーテーマを変えれば一緒に変わる**。
#[derive(Clone, Debug)]
struct Palette {
    fg: [Color32; Tok::COUNT],
    italic: [bool; Tok::COUNT],
    underline: [bool; Tok::COUNT],
}

impl Highlighter {
    pub fn new() -> Self {
        Self {
            ps: OnceLock::new(),
            rust_ps: OnceLock::new(),
            ts: OnceLock::new(),
            packs: RwLock::new(Arc::new(GrammarSet::default())),
            palettes: Mutex::new(HashMap::new()),
            cache: Mutex::new((
                HashMap::with_capacity(HL_CACHE_CAP),
                std::collections::VecDeque::with_capacity(HL_CACHE_CAP),
            )),
        }
    }

    /// 構文セット (初回アクセスで読み込む)。
    fn ps(&self) -> &SyntaxSet {
        self.ps.get_or_init(load_syntaxes)
    }

    /// 言語名から「塗るのに使う構文セットと構文」を選ぶ。
    ///
    /// Rust だけは同梱の最新定義 ([`load_rust_syntax`]) を持つ**別セット**を返す。
    /// `ParseState` / `HighlightLines` は自分が属するセットと組でしか使えないので、
    /// 構文と一緒にセットも返して取り違えを型で防ぐ。
    fn syntax_for(&self, lang: &str) -> Option<(&SyntaxSet, &SyntaxReference)> {
        if lang == "Rust" {
            if let Some(set) = self.rust_ps.get_or_init(load_rust_syntax).as_ref() {
                if let Some(s) = set.find_syntax_by_name(lang) {
                    return Some((set, s));
                }
            }
        }
        self.ps().find_syntax_by_name(lang).map(|s| (self.ps(), s))
    }

    /// テーマ集合 (初回アクセスで読み込む)。
    fn ts(&self) -> &ThemeSet {
        self.ts.get_or_init(load_themes)
    }

    /// いま読み込んでいる言語定義 (`Arc` の複製を返すので、呼び出し側は
    /// ロックを握ったまま走査しない)。
    fn packs(&self) -> Arc<GrammarSet> {
        match self.packs.read() {
            Ok(g) => g.clone(),
            // 毒されたロックでも「追加言語が無い」状態で描画は続ける。
            Err(e) => e.into_inner().clone(),
        }
    }

    /// プラグイン由来の言語定義を差し替える。折りたたみ用の [`LangSpec`] も
    /// ここで登録するので、以後 `lang_spec()` が追加言語を知っている状態になる。
    pub fn set_grammars(&self, packs: GrammarSet) {
        register_lang_specs(&packs);
        match self.packs.write() {
            Ok(mut g) => *g = Arc::new(packs),
            Err(e) => *e.into_inner() = Arc::new(packs),
        }
        // 既存のキャッシュは「その言語を知らなかった頃」の結果なので捨てる。
        if let Ok(mut g) = self.cache.lock() {
            g.0.clear();
            g.1.clear();
        }
    }

    /// 追加で認識できる言語の数 (プラグイン画面の表示用)。
    pub fn extra_lang_count(&self) -> usize {
        self.packs().grammars.len()
    }

    /// パス (と必要なら本文の 1 行目) から言語名を決める。
    ///
    /// **syntect の拡張セットを先に見る**。拡張セットの定義は本物の
    /// `.sublime-syntax` (文脈スタックを持ち、埋め込み言語も追える) で、
    /// パック側の正規表現近似より精度が高いためである。実例として
    /// `.vue` / `.svelte` はパックでは HTML 扱いだったが、拡張セットには
    /// 専用の Vue Component / Svelte がある。パックは
    /// **拡張セットが知らない拡張子だけを埋める**役に降りる。
    ///
    /// 例外は [`SYNTECT_MISASSIGNED`] に列挙した取り違えだけで、そこは
    /// パックを先に見る。
    pub fn lang_for(&self, path: Option<&Path>, text: &str) -> String {
        let packs = self.packs();
        if let Some(p) = path {
            // (1) ファイル名まるごと。`Dockerfile` / `Makefile` /
            // `CMakeLists.txt` / `.gitignore` / `.bashrc` のような
            // 「拡張子では決まらない」ものが拡張セットの `file_extensions`
            // に載っている。**拡張子より具体的なので先に見る**
            // (`CMakeLists.txt` を拡張子 `txt` = Plain Text と誤判定しないため)。
            if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                if let Some(s) = self
                    .ps()
                    .find_syntax_by_extension(n)
                    .filter(|s| s.name != PLAIN_TEXT)
                {
                    return s.name.clone();
                }
            }
            // (2) 拡張子。
            let ext = p.extension().and_then(|e| e.to_str());
            if let Some(e) = ext.filter(|e| !is_misassigned(e)) {
                if let Some(s) = self
                    .ps()
                    .find_syntax_by_extension(e)
                    .filter(|s| s.name != PLAIN_TEXT)
                {
                    return s.name.clone();
                }
            }
            // (3) 拡張セットが知らない拡張子をパックが埋める。
            if let Some(name) = packs.detect_path(p) {
                return name;
            }
            // (4) 取り違え扱いにした拡張子でも、パックが無ければ
            // (プラグインを切っている等) 素通しにせず拡張セットへ落とす。
            // ここは Plain Text も許す (`notes.txt` の行き先はそれで正しい)。
            if let Some(e) = ext {
                if let Some(s) = self.ps().find_syntax_by_extension(e) {
                    return s.name.clone();
                }
            }
        }
        if let Some(line) = text.lines().next() {
            if let Some(s) = self.ps().find_syntax_by_first_line(line) {
                return s.name.clone();
            }
            if let Some(name) = packs.detect_first_line(line) {
                return name;
            }
        }
        "Plain Text".into()
    }

    /// フェンスコードの言語トークン ("rust", "py" など) から言語名を引く。
    /// 優先順位は [`Self::lang_for`] と同じ (拡張セット → パック)。
    pub fn lang_for_fence(&self, token: &str) -> String {
        if token.is_empty() {
            return "Plain Text".into();
        }
        if !is_misassigned(token) {
            if let Some(s) = self.ps().find_syntax_by_token(token) {
                return s.name.clone();
            }
        }
        if let Some(name) = self.packs().detect_token(token) {
            return name;
        }
        self.ps()
            .find_syntax_by_token(token)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "Plain Text".into())
    }

    /// テーマ名からパレットを作る (作成済みなら使い回す)。
    fn palette(&self, theme_name: &str) -> Option<Arc<Palette>> {
        if let Ok(g) = self.palettes.lock() {
            if let Some(p) = g.get(theme_name) {
                return Some(p.clone());
            }
        }
        let theme = self.ts().themes.get(theme_name)?;
        let sh = SynHighlighter::new(theme);
        let default = sh.get_default();
        let mut pal = Palette {
            fg: [Color32::WHITE; Tok::COUNT],
            italic: [false; Tok::COUNT],
            underline: [false; Tok::COUNT],
        };
        for i in 0..Tok::COUNT {
            let tok = Tok::from_index(i);
            let style = Scope::new(tok.scope())
                .ok()
                .map(|s| sh.style_for_stack(&[s]))
                .unwrap_or(default);
            pal.fg[i] =
                Color32::from_rgb(style.foreground.r, style.foreground.g, style.foreground.b);
            pal.italic[i] = style.font_style.contains(FontStyle::ITALIC);
            pal.underline[i] = style.font_style.contains(FontStyle::UNDERLINE);
        }
        let arc = Arc::new(pal);
        if let Ok(mut g) = self.palettes.lock() {
            g.insert(theme_name.to_string(), arc.clone());
        }
        Some(arc)
    }

    pub fn layout_job(
        &self,
        text: &str,
        lang: &str,
        theme_name: &str,
        font: FontId,
        fallback: Color32,
    ) -> LayoutJob {
        // 全文を 1 度に塗るとフレームが止まる大きさなら、可視域だけを塗る
        // 経路へ落とす。画面のどこを見ているか知らないここでは先頭ブロックを
        // 塗る (可視域を渡せる呼び出し側は [`Self::layout_job_visible`] を使う)。
        // 本文のハッシュを取る**前**に分岐する — 巨大文書は `cache_put` が
        // 弾くので完成品キャッシュに載ることが無く、鍵を作るだけ無駄になる。
        if text.len() > MAX_HIGHLIGHT_BYTES {
            let v = self.layout_job_visible(
                text,
                lang,
                theme_name,
                font,
                fallback,
                snap_window(0, WINDOW_BLOCK_LINES),
            );
            // 先頭ブロックは 0 行目のスナップショットからそのまま塗れるので、
            // ここが暫定表示になることは無い。
            debug_assert!(v.exact, "先頭ブロックが暫定表示になっている");
            // 1 回の塗りは「チェックポイント間隔 + 可視域」で頭打ちになる。
            // ここが破れたら、遠い行へ飛んだときにフレームが止まる。
            debug_assert!(
                v.scanned_lines
                    <= WINDOW_SCAN_BUDGET_LINES + CHECKPOINT_LINES + 2 * WINDOW_BLOCK_LINES,
                "1 回の塗りが頭打ちを超えた: {}",
                v.scanned_lines
            );
            return v.job;
        }
        // キャッシュキーのハッシュ計算
        // 本文を**含まない**キー。行キャッシュ ([`DocCache`]) はこれが
        // 一致するときだけ再利用できる (言語やテーマが変われば色が変わるため)。
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        // 行キャッシュはスレッドローカル = インスタンス跨ぎなので、
        // どの [`Highlighter`] が作った結果かもキーに混ぜる。
        (self as *const Self as usize).hash(&mut hasher);
        lang.hash(&mut hasher);
        theme_name.hash(&mut hasher);
        font.hash(&mut hasher);
        fallback.hash(&mut hasher);
        let style_key = hasher.finish();
        // 完成品のキャッシュキーは本文も混ぜる。
        text.hash(&mut hasher);
        let key = hasher.finish();

        if let Ok(guard) = self.cache.lock() {
            if let Some(cached_job) = guard.0.get(&key) {
                return cached_job.clone();
            }
        }

        let plain = |job: &mut LayoutJob| {
            job.append(
                text,
                0.0,
                TextFormat {
                    font_id: font.clone(),
                    color: fallback,
                    ..Default::default()
                },
            );
        };

        let mut job = LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;

        if text.len() > MAX_HIGHLIGHT_BYTES {
            plain(&mut job);
            self.cache_put(key, &job);
            return job;
        }

        // syntect が知っている言語はそちらで塗る。知らない言語 (TypeScript /
        // Kotlin / Zig …) はプラグインのパックへ落とし、そこにも無ければ素の文字。
        let Some((set, syntax)) = self.syntax_for(lang).filter(|(_, s)| s.name != PLAIN_TEXT)
        else {
            let packs = self.packs();
            match (packs.by_name(lang), self.palette(theme_name)) {
                (Some(g), Some(pal)) => {
                    append_grammar(&mut job, text, g, &pal, &font, fallback);
                }
                _ => plain(&mut job),
            }
            self.cache_put(key, &job);
            return job;
        };

        let Some(theme) = self.ts().themes.get(theme_name) else {
            plain(&mut job);
            self.cache_put(key, &job);
            return job;
        };

        // 行ごとの塗り分けを (可能なら前回の結果を再利用して) 作り、
        // そこから LayoutJob を組む。**1 打鍵ぶんの再解析は変更点の周辺だけ**
        // で済むので、何万行のファイルでも編集が固まらない。
        let lines: Vec<&str> = LinesWithEndings::from(text).collect();
        job = DOC_CACHE.with(|c| {
            let mut slot = c.borrow_mut();
            let fresh = highlight_doc(set, syntax, theme, text, fallback, style_key, slot.as_ref());
            let job = job_from_spans(text, &lines, &fresh.spans, &font);
            if text.len() >= DOC_CACHE_MIN_BYTES {
                *slot = Some(fresh);
            }
            job
        });

        self.cache_put(key, &job);

        job
    }

    /// 巨大ファイルでも**可視域には必ず色を付ける**入口。
    ///
    /// `first_line` / `line_count` は画面に見えている行域。**[`snap_window`] を
    /// 通した値を渡し、呼び出し側の galley キャッシュキーにも同じ値を混ぜること**
    /// (生のスクロール位置を混ぜると 1 行動くたびに galley を組み直すことになり、
    /// 実測 495ms/回 が毎フレーム乗る)。
    ///
    /// [`MAX_HIGHLIGHT_BYTES`] 以下の文書は従来どおり全文を塗って返すので、
    /// `exact` は必ず `true` になる。
    pub fn layout_job_visible(
        &self,
        text: &str,
        lang: &str,
        theme_name: &str,
        font: FontId,
        fallback: Color32,
        win: Window,
    ) -> VisibleJob {
        if text.len() > MAX_HIGHLIGHT_BYTES {
            return self.visible_job(text, lang, theme_name, font, fallback, win);
        }
        VisibleJob {
            job: self.layout_job(text, lang, theme_name, font, fallback),
            exact: true,
            scanned_lines: 0,
        }
    }

    /// 可視域を**正しい文脈**で塗るための追い付きを 1 フレームぶん進める。
    ///
    /// `LayoutJob` を作らないので、呼び出し側は galley を組み直さなくてよい
    /// (組み直しは実測 495ms なので、追い付きのたびに組み直すと本末転倒)。
    /// 毎フレーム呼び、
    ///
    /// * `ready` が `false` のあいだは `ctx.request_repaint()` する
    /// * `false` → `true` に変わった 1 回だけ galley を捨てて塗り直す
    ///
    /// という使い方をする。**追い付き済みなら本文に触れずに即座に返る**ので、
    /// スクロールが止まっているあいだの費用はゼロになる。
    pub fn advance_to_visible(
        &self,
        text: &str,
        lang: &str,
        theme_name: &str,
        fallback: Color32,
        win: Window,
    ) -> Advance {
        if text.len() <= MAX_HIGHLIGHT_BYTES {
            return Advance {
                ready: true,
                scanned_lines: 0,
            };
        }
        let w0 = win.start;
        let Some(style_key) = self.window_style_key(lang, theme_name, fallback) else {
            // 構文もテーマも引けない = そもそも塗らないので、待つものが無い。
            return Advance {
                ready: true,
                scanned_lines: 0,
            };
        };
        WIN_CACHE.with(|c| {
            let mut slot = c.borrow_mut();
            // 追い付き済みなら本文を 1 バイトも読まずに返る (アイドル費用ゼロ)。
            // 本文が変わったかどうかの照合は、galley を作り直す側
            // (`visible_job`) が必ず通るのでここでは要らない。
            if let Some(wc) = slot.as_ref() {
                // `line_count` は本文の行数 (塗る側が毎回入れ直す)。可視域が
                // 末尾を越えているときに「永遠に届かない目標」を追いかけて
                // 毎フレーム全行を数え直さないためのもの。
                if wc.style_key == style_key && wc.frontier.cp.line >= w0.min(wc.line_count) {
                    return Advance {
                        ready: true,
                        scanned_lines: 0,
                    };
                }
            }
            let Some((set, syntax)) = self.syntax_for(lang).filter(|(_, s)| s.name != PLAIN_TEXT)
            else {
                return Advance {
                    ready: true,
                    scanned_lines: 0,
                };
            };
            let Some(theme) = self.ts().themes.get(theme_name) else {
                return Advance {
                    ready: true,
                    scanned_lines: 0,
                };
            };
            let hl = ThemeHighlighter::new(theme);
            let lines: Vec<&str> = LinesWithEndings::from(text).collect();
            let w0 = w0.min(lines.len());
            if !matches!(slot.as_ref(), Some(w) if w.style_key == style_key) {
                *slot = Some(WinCache::new(style_key, syntax, &hl));
            }
            let wc = slot.as_mut().expect("直前に入れた");
            wc.line_count = lines.len();
            let scanned = win_advance(set, syntax, &hl, &lines, wc, w0, WINDOW_SCAN_BUDGET_LINES);
            Advance {
                ready: wc.frontier.cp.line >= w0,
                scanned_lines: scanned,
            }
        })
    }

    /// 可視域だけを塗る本体 ([`MAX_HIGHLIGHT_BYTES`] 超え専用)。
    fn visible_job(
        &self,
        text: &str,
        lang: &str,
        theme_name: &str,
        font: FontId,
        fallback: Color32,
        win: Window,
    ) -> VisibleJob {
        let mut job = LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;
        job.text.push_str(text);
        let flat = |job: &mut LayoutJob| {
            if !text.is_empty() {
                push_section(job, 0..text.len(), fallback, false, false, &font);
            }
        };

        // 構文もテーマも引けないなら塗りようが無い (従来どおり素の文字)。
        let (Some((set, syntax)), Some(style_key)) = (
            self.syntax_for(lang).filter(|(_, s)| s.name != PLAIN_TEXT),
            self.window_style_key(lang, theme_name, fallback),
        ) else {
            flat(&mut job);
            return VisibleJob {
                job,
                exact: true,
                scanned_lines: 0,
            };
        };
        let Some(theme) = self.ts().themes.get(theme_name) else {
            flat(&mut job);
            return VisibleJob {
                job,
                exact: true,
                scanned_lines: 0,
            };
        };

        let hl = ThemeHighlighter::new(theme);
        let lines: Vec<&str> = LinesWithEndings::from(text).collect();
        let n = lines.len();
        let (w0, w1) = (win.start.min(n), win.end.min(n));
        if w0 >= w1 {
            flat(&mut job);
            return VisibleJob {
                job,
                exact: true,
                scanned_lines: 0,
            };
        }

        // 本文が変わっていたら、変わった所から先の足場を捨てる。ここは
        // 呼び出し側の galley キャッシュが外れた時 = 本文か可視域が動いた時
        // にしか通らないので、照合の費用 (先頭からフロンティアまでのハッシュ)
        // を毎フレーム払うことにはならない。
        WIN_CACHE.with(|c| {
            let mut slot = c.borrow_mut();
            if !matches!(slot.as_ref(), Some(w) if w.style_key == style_key) {
                *slot = Some(WinCache::new(style_key, syntax, &hl));
            }
            slot.as_mut().expect("直前に入れた").trim_to_unchanged(text);
        });
        // 追い付きを 1 回ぶん進める。**前進する経路はここ 1 本だけ**にして、
        // 「毎フレーム呼ぶ pump」と「塗るとき」で規則がずれないようにする。
        let adv = self.advance_to_visible(text, lang, theme_name, fallback, win);

        let (spans, exact, scanned) = WIN_CACHE.with(|c| {
            let mut slot = c.borrow_mut();
            let wc = slot.as_mut().expect("直前に入れた");
            wc.line_count = n;
            let mut scanned = adv.scanned_lines;
            // 再開点 = 可視域の手前で最も後ろのスナップショット。0 行目は
            // 必ず居るので `partition_point` は 1 以上を返す。
            let k = wc.points.partition_point(|p| p.cp.line <= w0) - 1;
            // 追い付き済みでも、再開点から可視域までが間隔を超えているなら
            // 「正確に塗る」を名乗らない。**1 回の塗りの費用を
            // `stride + 可視域` で頭打ちにする**のはこの条件そのもの。
            let exact = adv.ready && w0 - wc.points[k].cp.line <= wc.stride;
            let mut state = if exact {
                // 可視域の手前で最も後ろのスナップショットから再開し、
                // そこから可視域の先頭まで追い付く (最大 `stride` 行)。
                let k = wc.points.partition_point(|p| p.cp.line <= w0) - 1;
                let p = &wc.points[k];
                let mut st = (p.cp.ps.clone(), p.cp.hs.clone());
                // ここは最大 `stride` 行 (上の条件で保証されている)。
                for line in &lines[p.cp.line..w0] {
                    paint_line(set, syntax, &hl, line, &mut st, fallback);
                    scanned += 1;
                }
                st
            } else {
                // 暫定表示。可視域の先頭から解析し直すので、行を跨ぐコメントや
                // 生文字列の途中だと色がずれる。**それでも白一色にはしない** —
                // 追い付いた時点で `exact` が立ち、正しい色へ塗り替わる。
                // 「素の文字にする」のとは違い、キーワード・文字列・コメント・
                // 数値はこの時点で既に塗り分けられている。
                (
                    ParseState::new(syntax),
                    HighlightState::new(&hl, syntect::parsing::ScopeStack::new()),
                )
            };
            let mut out = Vec::with_capacity(w1 - w0);
            for line in &lines[w0..w1] {
                out.push(paint_line(set, syntax, &hl, line, &mut state, fallback));
                scanned += 1;
            }
            (out, exact, scanned)
        });

        // 可視域の外は 1 セクションにまとめる (画面に出ないので色は要らない。
        // セクション数は galley の費用にほとんど効かないが、作る費用は効く)。
        let off0: usize = lines[..w0].iter().map(|l| l.len()).sum();
        let off1: usize = off0 + lines[w0..w1].iter().map(|l| l.len()).sum::<usize>();
        if off0 > 0 {
            push_section(&mut job, 0..off0, fallback, false, false, &font);
        }
        append_spans(&mut job, off0, &lines[w0..w1], &spans, &font);
        if off1 < text.len() {
            push_section(&mut job, off1..text.len(), fallback, false, false, &font);
        }
        VisibleJob {
            job,
            exact,
            scanned_lines: scanned,
        }
    }

    /// チェックポイント台帳を作り直すかどうかを決める鍵。
    ///
    /// フォントは混ぜない — 塗り分け ([`LineSpan`]) は色と字体しか持たず
    /// フォントに依存しないので、文字サイズを変えただけで台帳を捨てると
    /// また先頭から舐め直しになる。構文もテーマも引けないときは `None`。
    fn window_style_key(&self, lang: &str, theme_name: &str, fallback: Color32) -> Option<u64> {
        self.syntax_for(lang)
            .filter(|(_, s)| s.name != PLAIN_TEXT)?;
        self.ts().themes.get(theme_name)?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (self as *const Self as usize).hash(&mut hasher);
        lang.hash(&mut hasher);
        theme_name.hash(&mut hasher);
        fallback.hash(&mut hasher);
        Some(hasher.finish())
    }

    /// 連続した行の並びを 1 パスで色分けし、**行ごとの (開始, 終了, 色)** を返す。
    ///
    /// 差分ビューのように「行を 1 本ずつ別ウィジェットで描く」画面のための API。
    /// 1 行ずつ [`Self::layout_job`] を呼ぶとキャッシュが行数ぶん溢れるうえ、
    /// 複数行にまたがる文字列やブロックコメントの状態が毎行リセットされて
    /// 色が壊れる。ここでは syntect の状態を行を跨いで持ち回る。
    ///
    /// 範囲は**各行の先頭を 0 とするバイトオフセット**の半開区間で、必ず
    /// 昇順・連続・行長に収まる (呼び出し側はそのまま `&line[s..e]` できる)。
    /// 合計が [`MAX_HIGHLIGHT_BYTES`] を超える / 言語が Plain Text /
    /// テーマが無い場合は**空の Vec** を返す (= 呼び出し側は素の色で描く)。
    pub fn line_spans(
        &self,
        lines: &[&str],
        lang: &str,
        theme_name: &str,
    ) -> Vec<Vec<(usize, usize, Color32)>> {
        let total: usize = lines.iter().map(|l| l.len() + 1).sum();
        let (set, syntax) = self
            .syntax_for(lang)
            .unwrap_or_else(|| (self.ps(), self.ps().find_syntax_plain_text()));
        if total > MAX_HIGHLIGHT_BYTES || syntax.name == PLAIN_TEXT {
            return Vec::new();
        }
        let Some(theme) = self.ts().themes.get(theme_name) else {
            return Vec::new();
        };
        let mut h = HighlightLines::new(syntax, theme);
        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            if line.len() > MAX_HIGHLIGHT_LINE_BYTES {
                // 極端に長い 1 行 (minify 済み JS など) で UI を止めない。
                out.push(Vec::new());
                continue;
            }
            // syntect は行末の改行込みで状態を進めるので付けて渡し、
            // 範囲は元の行長で丸める。
            let with_nl = format!("{line}\n");
            match h.highlight_line(&with_nl, set) {
                Ok(regions) => {
                    let mut spans: Vec<(usize, usize, Color32)> = Vec::with_capacity(regions.len());
                    let mut off = 0usize;
                    for (style, piece) in regions {
                        let start = off.min(line.len());
                        off += piece.len();
                        let end = off.min(line.len());
                        if end > start {
                            let fg = style.foreground;
                            spans.push((start, end, Color32::from_rgb(fg.r, fg.g, fg.b)));
                        }
                    }
                    out.push(spans);
                }
                Err(_) => {
                    // エラー後の HighlightLines は内部状態が壊れている可能性が
                    // あるので、以降の行のために作り直す。
                    h = HighlightLines::new(syntax, theme);
                    out.push(Vec::new());
                }
            }
        }
        out
    }

    /// キャッシュへ 1 件入れる。上限は古い方から追い出す (全消しすると
    /// 次のフレームに全再ハイライトのスパイクが出るため)。
    fn cache_put(&self, key: u64, job: &LayoutJob) {
        // 巨大な文書は行キャッシュ側で差分計算するので、完成品まで抱えない。
        if job.text.len() > JOB_CACHE_MAX_BYTES {
            return;
        }
        if let Ok(mut guard) = self.cache.lock() {
            let (map, order) = &mut *guard;
            while map.len() >= HL_CACHE_CAP {
                match order.pop_front() {
                    Some(old) => {
                        map.remove(&old);
                    }
                    None => {
                        map.clear();
                        break;
                    }
                }
            }
            if map.insert(key, job.clone()).is_none() {
                order.push_back(key);
            }
        }
    }
}

/// プラグイン定義の言語を 1 文書ぶん塗って `job` へ積む。
/// 走査は 1 行ごと ([`grammar::scan_line`]) で、行を跨ぐコメント・文字列は
/// `ScanState` が引き継ぐ。長すぎる 1 行は syntect 経路と同じく素通しにする。
fn append_grammar(
    job: &mut LayoutJob,
    text: &str,
    g: &Grammar,
    pal: &Palette,
    font: &FontId,
    fallback: Color32,
) {
    let mut st = ScanState::default();
    let mut spans: Vec<Span> = Vec::new();
    for line in LinesWithEndings::from(text) {
        if line.len() > MAX_HIGHLIGHT_LINE_BYTES {
            job.append(
                line,
                0.0,
                TextFormat {
                    font_id: font.clone(),
                    color: fallback,
                    ..Default::default()
                },
            );
            continue;
        }
        spans.clear();
        grammar::scan_line(g, line, &mut st, &mut spans);
        for s in &spans {
            let i = s.tok.index();
            let mut fmt = TextFormat {
                font_id: font.clone(),
                color: pal.fg[i],
                ..Default::default()
            };
            if pal.italic[i] {
                fmt.italics = true;
            }
            if pal.underline[i] {
                fmt.underline = eframe::egui::Stroke::new(1.0_f32, fmt.color);
            }
            job.append(&line[s.start..s.end], 0.0, fmt);
        }
    }
}

// ===========================================================================
// 構造解析レイヤ (折りたたみ / インデントガイド / スティッキースクロール)
//
// ここは **描画を一切しない純関数の層**。app.rs 側の描画コードは
// `fold_ranges` / `indent_guides` / `active_guide` / `sticky_headers` を
// 呼ぶだけでよく、言語ごとの知識 (コメント記号・括弧・見出し) は
// すべて [`LANG_SPECS`] という 1 枚のデータ表に閉じ込めてある。
// 新しい言語を足すときは表に 1 行足すだけで、関数側は触らない。
//
// --- UI (app.rs) 側の配線の手引き -------------------------------------------
//
// 行番号はすべて **0 始まり** (`text.split('\n')` の添字)。`Editor::cursor` は
// 1 始まりなので、渡す前に 1 を引くこと。
//
// * 折りたたみ: 毎フレーム `buf.refresh_folds()` を呼ぶ (本文が変わった
//   ときだけ中で再計算する)。ガターは `buf.folds.marker(line)` で ▼/▶ を
//   描き、クリックで `buf.folds.toggle_fold(line)`。描画時は
//   `buf.folds.hidden_spans()` で隠す行区間をまとめて取る。
//   編集で行が増減したら `buf.folds.shift_lines(at, delta)` を先に呼ぶと
//   畳んだ状態が編集を跨いで生き残る。
// * インデントガイド: `indent_guides(&buf.text, tab_width)[line].1` が
//   その行で縦線を引く桁位置。`active_guide(&buf.text, tab_width, caret)`
//   の桁だけ色を変えると VS Code と同じ見た目になる。
// * スティッキースクロール: `sticky_headers(&buf.text, &buf.lang,
//   最上部の可視行, 最大段数)` を上端に重ねて描く。
// ===========================================================================

/// 折りたたみ範囲の計算方式。言語ごとに [`LANG_SPECS`] で選ぶ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldStrategy {
    /// 括弧の対応で畳む (C 系)。文字列・コメントの中の括弧は数えない。
    Brackets,
    /// インデントの深さで畳む (Python / YAML / 既定)。どの言語でも動く。
    Indent,
    /// 見出しの階層で畳む (Markdown)。インデントも併用する。
    Markdown,
}

/// 折りたたみ範囲の種類。UI はこれでガターの記号や色を変えられる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldKind {
    /// 括弧 / フェンスで囲まれたブロック。
    Block,
    /// インデントで作られたブロック。
    Indent,
    /// 複数行コメント (ブロックコメント、または連続した行コメント)。
    Comment,
    /// Markdown の見出し節。
    Section,
}

/// 折りたたみ 1 個ぶんの範囲。
///
/// **契約**: `start_line` は畳んでも**表示され続ける**行 (ヘッダ行)。
/// `end_line` は畳んだときに隠れる**最後の行**。よって隠れるのは
/// `start_line+1 ..= end_line` で、`end_line > start_line` が常に成り立つ。
/// 行番号はすべて 0 始まり (`text.split('\n')` の添字)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldRange {
    pub start_line: usize,
    pub end_line: usize,
    pub kind: FoldKind,
}

#[allow(dead_code)]
impl FoldRange {
    /// この範囲を畳んだときに `line` が隠れるか。
    pub fn hides(&self, line: usize) -> bool {
        line > self.start_line && line <= self.end_line
    }
    /// 隠れる行数。
    pub fn hidden_len(&self) -> usize {
        self.end_line - self.start_line
    }
}

/// 言語ごとの構文知識。**リテラルを関数側に散らさないための唯一の置き場**。
pub struct LangSpec {
    /// syntect の syntax 名 (大文字小文字は無視して完全一致で引く)。
    pub names: &'static [&'static str],
    /// 折りたたみの計算方式。
    pub strategy: FoldStrategy,
    /// 行コメントの開始トークン。
    pub line_comment: &'static [&'static str],
    /// ブロックコメントの (開始, 終了) トークン。
    pub block_comment: &'static [(&'static str, &'static str)],
    /// 折りたたみに使う括弧の (開き, 閉じ)。
    pub brackets: &'static [(char, char)],
    /// 文字列リテラルの引用符 (この中の括弧・コメントは無視する)。
    pub quotes: &'static [char],
    /// 文字列中のエスケープ文字。
    pub escape: Option<char>,
}

// --- 表の中で使い回す共通の集合 (重複リテラルを 1 か所に) ---
const BR_CURLY: &[(char, char)] = &[('{', '}'), ('[', ']'), ('(', ')')];
const BR_PAREN: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}')];
const CM_SLASH: &[&str] = &["//"];
const CM_HASH: &[&str] = &["#"];
const CM_DASH: &[&str] = &["--"];
const CM_PERCENT: &[&str] = &["%"];
const CM_SEMI: &[&str] = &[";"];
const CM_HASH_SEMI: &[&str] = &["#", ";"];
const BC_C: &[(&str, &str)] = &[("/*", "*/")];
const BC_XML: &[(&str, &str)] = &[("<!--", "-->")];
const BC_NONE: &[(&str, &str)] = &[];
const CM_NONE: &[&str] = &[];
const Q_D: &[char] = &['"'];
const Q_DS: &[char] = &['"', '\''];
const Q_NONE: &[char] = &[];
const ESC: Option<char> = Some('\\');

/// 言語データ表。**上から順に最初に名前が一致したものを使う**。
///
/// 名前は syntect の `SyntaxSet::load_defaults_newlines()` が返す syntax 名。
/// 表にない言語は [`DEFAULT_LANG_SPEC`] (インデント方式) にフォールバックするので、
/// 未知の言語でも折りたたみは必ず動く。
pub static LANG_SPECS: &[LangSpec] = &[
    // Rust は `'a` がライフタイムなので、シングルクォートを文字列扱いしない。
    LangSpec {
        names: &["Rust"],
        strategy: FoldStrategy::Brackets,
        line_comment: CM_SLASH,
        block_comment: BC_C,
        brackets: BR_CURLY,
        quotes: Q_D,
        escape: ESC,
    },
    // C 系 (シングルクォートは文字リテラル)
    LangSpec {
        names: &[
            "C",
            "C++",
            "C#",
            "D",
            "Go",
            "Java",
            "Javascript",
            "JavaScript",
            "TypeScript",
            "TypeScriptReact",
            "JavaScript (Babel)",
            "JavaScript (Rails)",
            "Java Server Page (JSP)",
            "Objective-C",
            "Objective-C++",
            "PHP",
            "Scala",
            "Groovy",
            "Swift",
            "Kotlin",
            "Dart",
            "Pascal",
            "Vala",
            "Zig",
        ],
        strategy: FoldStrategy::Brackets,
        line_comment: CM_SLASH,
        block_comment: BC_C,
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    // JSON / JSONC (この製品は jsonc.rs でコメント付き JSON を読む)
    LangSpec {
        names: &["JSON", "JSONC", "JSON5"],
        strategy: FoldStrategy::Brackets,
        line_comment: CM_SLASH,
        block_comment: BC_C,
        brackets: BR_CURLY,
        quotes: Q_D,
        escape: ESC,
    },
    LangSpec {
        names: &["CSS", "SCSS", "Sass", "LESS", "Stylus"],
        strategy: FoldStrategy::Brackets,
        line_comment: CM_SLASH,
        block_comment: BC_C,
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["Perl"],
        strategy: FoldStrategy::Brackets,
        line_comment: CM_HASH,
        block_comment: BC_NONE,
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["R", "Rd (R Documentation)"],
        strategy: FoldStrategy::Brackets,
        line_comment: CM_HASH,
        block_comment: BC_NONE,
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["Lisp", "Clojure", "Scheme", "Emacs Lisp"],
        strategy: FoldStrategy::Brackets,
        line_comment: CM_SEMI,
        block_comment: BC_NONE,
        brackets: BR_PAREN,
        quotes: Q_D,
        escape: ESC,
    },
    // Python: 三重引用符は「複数行コメント」として畳む (docstring)
    LangSpec {
        names: &["Python"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_HASH,
        block_comment: &[("\"\"\"", "\"\"\""), ("'''", "'''")],
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["YAML", "YAML Front Matter"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_HASH,
        block_comment: BC_NONE,
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["TOML", "INI", "Java Properties", "Git Config"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_HASH_SEMI,
        block_comment: BC_NONE,
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &[
            "Shell-Unix-Generic",
            "Bourne Again Shell (bash)",
            "Batch File",
            "Makefile",
            "Dockerfile",
            "CMake",
        ],
        strategy: FoldStrategy::Indent,
        line_comment: CM_HASH,
        block_comment: BC_NONE,
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["Ruby", "Ruby on Rails", "Ruby Haml"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_HASH,
        block_comment: &[("=begin", "=end")],
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["Lua"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_DASH,
        block_comment: &[("--[[", "]]")],
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["SQL", "SQL (Rails)"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_DASH,
        block_comment: BC_C,
        brackets: BR_PAREN,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["Haskell", "Literate Haskell"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_DASH,
        block_comment: &[("{-", "-}")],
        brackets: BR_CURLY,
        quotes: Q_D,
        escape: ESC,
    },
    LangSpec {
        names: &["Erlang", "MATLAB", "LaTeX", "TeX", "BibTeX"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_PERCENT,
        block_comment: BC_NONE,
        brackets: BR_CURLY,
        quotes: Q_D,
        escape: ESC,
    },
    LangSpec {
        names: &[
            "HTML",
            "XML",
            "HTML (Rails)",
            "HTML (ASP)",
            "HTML (Tcl)",
            "SVG",
            "XSL",
        ],
        strategy: FoldStrategy::Indent,
        line_comment: CM_NONE,
        block_comment: BC_XML,
        brackets: BR_CURLY,
        quotes: Q_DS,
        escape: ESC,
    },
    LangSpec {
        names: &["Markdown", "MultiMarkdown", "Markdown (GFM)"],
        strategy: FoldStrategy::Markdown,
        line_comment: CM_NONE,
        block_comment: BC_XML,
        brackets: BR_CURLY,
        quotes: Q_NONE,
        escape: None,
    },
    // 差分ビューはコメントも括弧も無い (`-` で始まる行を行コメント扱いしない)
    LangSpec {
        names: &["Diff", "Cargo Build Results"],
        strategy: FoldStrategy::Indent,
        line_comment: CM_NONE,
        block_comment: BC_NONE,
        brackets: BR_CURLY,
        quotes: Q_NONE,
        escape: None,
    },
];

/// 表に無い言語のフォールバック。インデント方式なのでどの言語でも壊れない。
pub static DEFAULT_LANG_SPEC: LangSpec = LangSpec {
    names: &["Plain Text"],
    strategy: FoldStrategy::Indent,
    line_comment: CM_NONE,
    block_comment: BC_NONE,
    brackets: BR_CURLY,
    quotes: Q_NONE,
    escape: None,
};

/// プラグイン定義の言語ぶんの [`LangSpec`]。文法データ ([`Grammar`]) から作り、
/// **言語名につき 1 回だけ**確保して `&'static` に固定する。プラグインの
/// 読み直しで何度 `set_grammars` が呼ばれても、同じ名前なら作り直さない。
static DYNAMIC_SPECS: std::sync::OnceLock<Mutex<HashMap<String, &'static LangSpec>>> =
    std::sync::OnceLock::new();

fn dynamic_specs() -> &'static Mutex<HashMap<String, &'static LangSpec>> {
    DYNAMIC_SPECS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn leak_str(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// 文法データから折りたたみ用の言語仕様を作る。
fn spec_from_grammar(g: &Grammar) -> LangSpec {
    let names: &'static [&'static str] = Box::leak(vec![leak_str(&g.name)].into_boxed_slice());
    let line_comment: &'static [&'static str] = Box::leak(
        g.line_comment
            .iter()
            .map(|s| leak_str(s))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let block_comment: &'static [(&'static str, &'static str)] = Box::leak(
        g.block_comment
            .iter()
            .chain(g.doc_block.iter())
            .map(|(a, b)| (leak_str(a), leak_str(b)))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    // 折りたたみは「文字列の中の括弧を数えない」ためだけに引用符を見る。
    // 1 文字の開き記号だけを渡せば足りる。
    let mut qs: Vec<char> = g
        .strings
        .iter()
        .filter_map(|r| {
            let mut it = r.open.chars();
            match (it.next(), it.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            }
        })
        .collect();
    qs.sort_unstable();
    qs.dedup();
    let quotes: &'static [char] = Box::leak(qs.into_boxed_slice());
    LangSpec {
        names,
        strategy: match g.fold {
            FoldKindSpec::Brackets => FoldStrategy::Brackets,
            FoldKindSpec::Indent => FoldStrategy::Indent,
            FoldKindSpec::Markdown => FoldStrategy::Markdown,
        },
        line_comment,
        block_comment,
        brackets: BR_CURLY,
        quotes,
        escape: g.escape,
    }
}

/// プラグインの言語定義を折りたたみ側へ登録する ([`Highlighter::set_grammars`] から)。
pub fn register_lang_specs(set: &GrammarSet) {
    let Ok(mut map) = dynamic_specs().lock() else {
        return;
    };
    for g in &set.grammars {
        let key = g.name.to_lowercase();
        if map.contains_key(&key) {
            continue;
        }
        let spec: &'static LangSpec = Box::leak(Box::new(spec_from_grammar(g)));
        map.insert(key, spec);
    }
}

/// プラグイン定義の言語の行コメント記号。**組み込みの表は見ない**ので、
/// 既存言語のコメント切り替えの挙動は変わらない (CSS の `//` のように、
/// 折りたたみ用には使うがコメント挿入には使いたくない記号があるため)。
pub fn dynamic_line_comment(lang: &str) -> Option<&'static str> {
    let map = dynamic_specs().lock().ok()?;
    let spec = map.get(&lang.to_lowercase())?;
    spec.line_comment.first().copied()
}

/// syntax 名から言語仕様を引く。**組み込みの表 → プラグイン定義**の順に見る
/// (組み込みを先に見るので、既存言語の挙動はプラグインで変わらない)。
/// どちらにも無ければ [`DEFAULT_LANG_SPEC`]。
pub fn lang_spec(lang: &str) -> &'static LangSpec {
    if let Some(s) = LANG_SPECS
        .iter()
        .find(|s| s.names.iter().any(|n| n.eq_ignore_ascii_case(lang)))
    {
        return s;
    }
    if let Ok(map) = dynamic_specs().lock() {
        if let Some(s) = map.get(&lang.to_lowercase()) {
            return s;
        }
    }
    &DEFAULT_LANG_SPEC
}

/// インデント幅の既定値 (タブ 1 個ぶんの桁数)。UI が設定値を持つなら
/// `fold_ranges_with` / `indent_guides` にそれを渡す。
pub const DEFAULT_TAB_WIDTH: usize = 4;

/// 構造解析を諦める行数。これを超えるファイルは巨大ファイルモード
/// (editor.rs) で読み取り専用になるため、折りたたみ計算は省く。
pub const FOLD_MAX_LINES: usize = 200_000;

/// 1 行の見た目のインデント桁数。空白だけの行は `None` (空行)。
/// タブは次の `tab_width` の倍数まで進む (エディタの表示と同じ勘定)。
fn visual_indent(line: &str, tab_width: usize) -> Option<usize> {
    let tw = tab_width.max(1);
    let mut col = 0usize;
    for ch in line.chars() {
        match ch {
            ' ' => col += 1,
            '\t' => col = (col / tw + 1) * tw,
            '\r' => {}
            _ => return Some(col),
        }
    }
    None
}

/// 字句スキャンの結果。コメント・括弧の位置だけを持つ最小限の情報。
#[derive(Default)]
struct SourceScan {
    /// 行コメントだけの行か (行ごと)。
    comment_only: Vec<bool>,
    /// 複数行にまたがるブロックコメントの (開始行, 終了行)。
    block_comments: Vec<(usize, usize)>,
    /// 対応の取れた括弧の (開き行, 閉じ行)。同一行のものは含まない。
    brackets: Vec<(usize, usize)>,
}

/// [`scan_lex`] が吐くできごと。**文字列リテラルとコメントの中は出てこない。**
enum Lex {
    /// 括弧の開き: (本文先頭からのバイト位置, 行, 対応する閉じ括弧)
    Open(usize, usize, char),
    /// 括弧の閉じ: (本文先頭からのバイト位置, 行, 文字)
    Close(usize, usize, char),
    /// この行で行コメントが始まった (行)
    LineComment(usize),
    /// この行にコード (空白・コメント以外) があった (行)
    Code(usize),
    /// 行を跨いだブロックコメント (開始行, 終了行)
    Block(usize, usize),
}

/// 文字列・コメントを飛ばしながら 1 パスで走査し、できごとを `f` へ流す。
///
/// **このクレートで「ソースの字句を追う」のはここ 1 か所だけ**。
/// 折りたたみ ([`scan_source`]) も虹色括弧 ([`bracket_pairs`]) もここを通るので、
/// 「文字列やコメントの中の `{` は数えない」という規則が 1 か所で決まる。
///
/// 単一行文字列を前提にしている (行末で強制的に閉じる)。Rust の生文字列
/// `r#".."#` や JS のテンプレートリテラルの複数行は正確に追えないが、
/// 折りたたみと色が少しずれるだけで壊れはしない。
fn scan_lex(text: &str, spec: &LangSpec, mut f: impl FnMut(Lex)) {
    // (終了トークン, 開始行)
    let mut in_block: Option<(&'static str, usize)> = None;
    let mut last_line = 0usize;
    // 行頭の絶対バイト位置
    let mut base = 0usize;
    for (ln, raw) in text.split('\n').enumerate() {
        last_line = ln;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let mut i = 0usize;
        while i < line.len() {
            let rest = &line[i..];
            if let Some((close, start)) = in_block {
                match rest.find(close) {
                    Some(p) => {
                        if ln > start {
                            f(Lex::Block(start, ln));
                        }
                        in_block = None;
                        i += p + close.len();
                        continue;
                    }
                    None => break,
                }
            }
            let ch = match rest.chars().next() {
                Some(c) => c,
                None => break,
            };
            if ch.is_whitespace() {
                i += ch.len_utf8();
                continue;
            }
            if spec.line_comment.iter().any(|t| rest.starts_with(*t)) {
                f(Lex::LineComment(ln));
                break;
            }
            if let Some(bc) = spec.block_comment.iter().find(|p| rest.starts_with(p.0)) {
                in_block = Some((bc.1, ln));
                i += bc.0.len();
                continue;
            }
            if spec.quotes.contains(&ch) {
                f(Lex::Code(ln));
                i += ch.len_utf8();
                while i < line.len() {
                    let c = match line[i..].chars().next() {
                        Some(c) => c,
                        None => break,
                    };
                    i += c.len_utf8();
                    if Some(c) == spec.escape {
                        if let Some(n) = line[i..].chars().next() {
                            i += n.len_utf8();
                        }
                        continue;
                    }
                    if c == ch {
                        break;
                    }
                }
                continue;
            }
            if let Some(b) = spec.brackets.iter().find(|p| p.0 == ch) {
                f(Lex::Code(ln));
                f(Lex::Open(base + i, ln, b.1));
                i += ch.len_utf8();
                continue;
            }
            if spec.brackets.iter().any(|p| p.1 == ch) {
                f(Lex::Code(ln));
                f(Lex::Close(base + i, ln, ch));
                i += ch.len_utf8();
                continue;
            }
            f(Lex::Code(ln));
            i += ch.len_utf8();
        }
        base += raw.len() + 1;
    }
    // 閉じられていないブロックコメントは末尾まで畳めるようにする
    if let Some((_, start)) = in_block {
        if last_line > start {
            f(Lex::Block(start, last_line));
        }
    }
}

/// 文字列・コメントを飛ばしながら 1 パスで走査する ([`scan_lex`] の集計)。
fn scan_source(text: &str, spec: &LangSpec) -> SourceScan {
    let n = text.split('\n').count();
    let mut out = SourceScan::default();
    let mut saw_code = vec![false; n];
    let mut saw_line_comment = vec![false; n];
    // (期待する閉じ括弧, 開いた行)
    let mut stack: Vec<(char, usize)> = Vec::new();
    scan_lex(text, spec, |ev| match ev {
        Lex::Open(_, ln, close) => stack.push((close, ln)),
        Lex::Close(_, ln, ch) => {
            // 対応する開きを探す。見つからない/入れ違いは黙って捨てる
            // (壊れたソースでも panic しないことを最優先)。
            if let Some(pos) = stack.iter().rposition(|p| p.0 == ch) {
                let open_ln = stack[pos].1;
                stack.truncate(pos);
                if ln > open_ln {
                    out.brackets.push((open_ln, ln));
                }
            }
        }
        Lex::LineComment(ln) => saw_line_comment[ln] = true,
        Lex::Code(ln) => saw_code[ln] = true,
        Lex::Block(s, e) => out.block_comments.push((s, e)),
    });
    out.comment_only = saw_line_comment
        .iter()
        .zip(&saw_code)
        .map(|(c, k)| *c && !*k)
        .collect();
    out
}

// ───────────────── 虹色括弧 (bracket pair colorization) ─────────────────

/// 括弧 1 個ぶんの色付け情報。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BracketHit {
    /// 本文先頭からのバイト位置。括弧は必ず 1 バイトの ASCII なので、
    /// `byte..byte + 1` が常に文字境界に収まる (CJK を含む行でも割れない)。
    pub byte: usize,
    /// 入れ子の深さ (一番外側が 0)。相手が居ないときは 0。
    pub depth: usize,
    /// 相手が見つからなかった括弧か (エラー色で描く)。
    pub unmatched: bool,
}

/// 本文から「深さ付きの括弧の位置」を拾う (VS Code の
/// `editor.bracketPairColorization`)。
///
/// 走査は [`scan_lex`] 1 本なので、**文字列リテラルとコメントの中の括弧は
/// 数えない**。対応の取れない括弧は `unmatched` が立つ。
/// 巨大ファイル ([`MAX_HIGHLIGHT_BYTES`] 超) は空を返す — 強調表示自体を
/// 止めている本文で括弧だけ走査しても意味が無いため。
///
/// 返り値はバイト位置の昇順。
pub fn bracket_pairs(text: &str, lang: &str) -> Vec<BracketHit> {
    if text.len() > MAX_HIGHLIGHT_BYTES {
        return Vec::new();
    }
    let spec = lang_spec(lang);
    if spec.brackets.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<BracketHit> = Vec::new();
    // (期待する閉じ括弧, out の添字)
    let mut stack: Vec<(char, usize)> = Vec::new();
    scan_lex(text, spec, |ev| match ev {
        Lex::Open(byte, _, close) => {
            // いったん「相手なし」で積み、閉じが来たら降ろす
            out.push(BracketHit {
                byte,
                depth: stack.len(),
                unmatched: true,
            });
            stack.push((close, out.len() - 1));
        }
        Lex::Close(byte, _, ch) => match stack.iter().rposition(|p| p.0 == ch) {
            Some(pos) => {
                let oi = stack[pos].1;
                // 途中で放置された開きは unmatched のまま捨てる
                stack.truncate(pos);
                out[oi].unmatched = false;
                let depth = out[oi].depth;
                out.push(BracketHit {
                    byte,
                    depth,
                    unmatched: false,
                });
            }
            None => out.push(BracketHit {
                byte,
                depth: 0,
                unmatched: true,
            }),
        },
        _ => {}
    });
    out.sort_by_key(|h| h.byte);
    out
}

/// [`LayoutJob`] の括弧 1 文字ずつを深さの色へ塗り替える。
///
/// 節 (section) を括弧の前後で割って色だけ差し替えるので、
/// **本文の 1 バイトも動かさない** (バイト範囲は連続・昇順のまま)。
/// `colors` が空なら何もしない。
pub fn colorize_brackets(
    mut job: LayoutJob,
    hits: &[BracketHit],
    colors: &[Color32],
    err: Color32,
) -> LayoutJob {
    if hits.is_empty() || colors.is_empty() {
        return job;
    }
    let mut out: Vec<eframe::egui::text::LayoutSection> =
        Vec::with_capacity(job.sections.len() + hits.len() * 2);
    // hits は昇順なので、節を進めながら添字も前へ進めるだけで足りる
    let mut hi = 0usize;
    for sec in job.sections.drain(..) {
        let (start, end) = (sec.byte_range.start, sec.byte_range.end);
        while hi < hits.len() && hits[hi].byte < start {
            hi += 1;
        }
        if hi >= hits.len() || hits[hi].byte >= end {
            out.push(sec);
            continue;
        }
        let mut cur = start;
        let mut k = hi;
        while k < hits.len() && hits[k].byte < end {
            let b = hits[k].byte;
            k += 1;
            if b < cur || b + 1 > end {
                continue;
            }
            if b > cur {
                out.push(eframe::egui::text::LayoutSection {
                    leading_space: if cur == start { sec.leading_space } else { 0.0 },
                    byte_range: cur..b,
                    format: sec.format.clone(),
                });
            }
            let mut fmt = sec.format.clone();
            fmt.color = if hits[k - 1].unmatched {
                err
            } else {
                colors[hits[k - 1].depth % colors.len()]
            };
            // 下線が付いている節では下線の色も合わせる (色だけ浮かないように)
            if fmt.underline.width > 0.0 {
                fmt.underline.color = fmt.color;
            }
            out.push(eframe::egui::text::LayoutSection {
                leading_space: if cur == start { sec.leading_space } else { 0.0 },
                byte_range: b..b + 1,
                format: fmt,
            });
            cur = b + 1;
        }
        if cur < end {
            out.push(eframe::egui::text::LayoutSection {
                leading_space: if cur == start { sec.leading_space } else { 0.0 },
                byte_range: cur..end,
                format: sec.format.clone(),
            });
        }
    }
    job.sections = out;
    job
}

/// インデント方式の折りたたみ範囲を積む。O(行数)。
///
/// 「自分より深い行が続く行」がヘッダになり、末尾の空行は範囲に含めない
/// (空行まで畳むと、次のブロックとの間の余白まで消えて読みにくいため)。
fn indent_folds(lines: &[&str], tab_width: usize, out: &mut Vec<FoldRange>) {
    let ind: Vec<Option<usize>> = lines.iter().map(|l| visual_indent(l, tab_width)).collect();
    // (インデント, 行)
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut prev_nonblank: Option<usize> = None;
    let mut close = |stack: &mut Vec<(usize, usize)>, upto: usize, prev: Option<usize>| {
        while let Some(&(ti, tl)) = stack.last() {
            if ti >= upto {
                stack.pop();
                if let Some(p) = prev {
                    if p > tl {
                        out.push(FoldRange {
                            start_line: tl,
                            end_line: p,
                            kind: FoldKind::Indent,
                        });
                    }
                }
            } else {
                break;
            }
        }
    };
    for (ln, cur) in ind.iter().enumerate() {
        let Some(a) = *cur else { continue };
        close(&mut stack, a, prev_nonblank);
        stack.push((a, ln));
        prev_nonblank = Some(ln);
    }
    // 末尾まで来たら全部閉じる (深さ 0 以上は必ず条件を満たす)。
    close(&mut stack, 0, prev_nonblank);
}

/// ATX 見出しの階層 (`#` の数)。見出しでなければ `None`。
fn heading_level(line: &str) -> Option<usize> {
    let t = line.strip_suffix('\r').unwrap_or(line);
    let lead = t.len() - t.trim_start_matches(' ').len();
    if lead > 3 {
        return None;
    }
    let t = &t[lead..];
    let hashes = t.len() - t.trim_start_matches('#').len();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    match t[hashes..].chars().next() {
        None | Some(' ') | Some('\t') => Some(hashes),
        _ => None,
    }
}

/// コードフェンスの開始トークン (``` または ~~~)。
fn fence_token(line: &str) -> Option<&'static str> {
    let t = line.trim_start();
    if t.starts_with("```") {
        Some("```")
    } else if t.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// Markdown の見出し節とコードフェンスを積む。
fn markdown_folds(lines: &[&str], out: &mut Vec<FoldRange>) {
    let mut fence: Option<(usize, &'static str)> = None;
    // (見出しレベル, 行)
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut last_content: Option<usize> = None;
    for (ln, raw) in lines.iter().enumerate() {
        if let Some((fl, tok)) = fence {
            last_content = Some(ln);
            if raw.trim_start().starts_with(tok) {
                if ln > fl {
                    out.push(FoldRange {
                        start_line: fl,
                        end_line: ln,
                        kind: FoldKind::Block,
                    });
                }
                fence = None;
            }
            continue;
        }
        if let Some(tok) = fence_token(raw) {
            fence = Some((ln, tok));
            last_content = Some(ln);
            continue;
        }
        if let Some(level) = heading_level(raw) {
            while let Some(&(l, sl)) = stack.last() {
                if l >= level {
                    stack.pop();
                    if let Some(c) = last_content {
                        if c > sl {
                            out.push(FoldRange {
                                start_line: sl,
                                end_line: c,
                                kind: FoldKind::Section,
                            });
                        }
                    }
                } else {
                    break;
                }
            }
            stack.push((level, ln));
            last_content = Some(ln);
            continue;
        }
        if !raw.trim().is_empty() {
            last_content = Some(ln);
        }
    }
    if let Some((fl, _)) = fence {
        // 閉じ忘れフェンスは末尾まで
        if lines.len() > fl + 1 {
            out.push(FoldRange {
                start_line: fl,
                end_line: lines.len() - 1,
                kind: FoldKind::Block,
            });
        }
    }
    while let Some((_, sl)) = stack.pop() {
        if let Some(c) = last_content {
            if c > sl {
                out.push(FoldRange {
                    start_line: sl,
                    end_line: c,
                    kind: FoldKind::Section,
                });
            }
        }
    }
}

/// 同じ開始行の範囲は**いちばん広いものだけ**残し、開始行の昇順に並べる。
fn normalize_folds(mut v: Vec<FoldRange>, line_count: usize) -> Vec<FoldRange> {
    v.retain(|r| r.end_line > r.start_line && r.start_line + 1 < line_count);
    for r in v.iter_mut() {
        r.end_line = r.end_line.min(line_count.saturating_sub(1));
    }
    v.sort_by(|a, b| {
        a.start_line
            .cmp(&b.start_line)
            .then(b.end_line.cmp(&a.end_line))
    });
    v.dedup_by_key(|r| r.start_line);
    v
}

/// バッファ全体の折りたたみ範囲を計算する (既定のタブ幅)。
///
/// - 行番号は 0 始まり。
/// - 同じ開始行の範囲は 1 個だけ (いちばん広いもの)。
/// - 開始行の昇順。入れ子は「外側が先」に並ぶ。
pub fn fold_ranges(text: &str, lang: &str) -> Vec<FoldRange> {
    fold_ranges_with(text, lang, DEFAULT_TAB_WIDTH)
}

/// タブ幅を指定して折りたたみ範囲を計算する。
pub fn fold_ranges_with(text: &str, lang: &str, tab_width: usize) -> Vec<FoldRange> {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() > FOLD_MAX_LINES {
        return Vec::new();
    }
    let spec = lang_spec(lang);
    let scan = scan_source(text, spec);
    let mut out: Vec<FoldRange> = Vec::new();
    for (s, e) in scan.block_comments.iter().copied() {
        out.push(FoldRange {
            start_line: s,
            end_line: e,
            kind: FoldKind::Comment,
        });
    }
    // 連続した行コメントの塊もひとまとめに畳めるようにする (VS Code と同じ)
    let n = scan.comment_only.len();
    let mut i = 0usize;
    while i < n {
        if scan.comment_only[i] {
            let mut j = i;
            while j + 1 < n && scan.comment_only[j + 1] {
                j += 1;
            }
            if j > i {
                out.push(FoldRange {
                    start_line: i,
                    end_line: j,
                    kind: FoldKind::Comment,
                });
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    match spec.strategy {
        FoldStrategy::Brackets => {
            for (s, e) in scan.brackets.iter().copied() {
                out.push(FoldRange {
                    start_line: s,
                    end_line: e,
                    kind: FoldKind::Block,
                });
            }
        }
        FoldStrategy::Indent => indent_folds(&lines, tab_width, &mut out),
        FoldStrategy::Markdown => {
            markdown_folds(&lines, &mut out);
            indent_folds(&lines, tab_width, &mut out);
        }
    }
    normalize_folds(out, lines.len())
}

/// 各行の「実効インデント」。空行は前後の非空行の**浅い方**を継ぐ
/// (ブロックの切れ目でガイドが宙に浮かないようにするため)。
fn effective_indents(lines: &[&str], tab_width: usize) -> Vec<usize> {
    let raw: Vec<Option<usize>> = lines.iter().map(|l| visual_indent(l, tab_width)).collect();
    let n = raw.len();
    let mut before = vec![0usize; n];
    let mut cur = 0usize;
    let mut seen = false;
    for i in 0..n {
        match raw[i] {
            Some(v) => {
                cur = v;
                seen = true;
                before[i] = v;
            }
            None => before[i] = if seen { cur } else { 0 },
        }
    }
    let mut after = vec![0usize; n];
    cur = 0;
    seen = false;
    for i in (0..n).rev() {
        match raw[i] {
            Some(v) => {
                cur = v;
                seen = true;
                after[i] = v;
            }
            None => after[i] = if seen { cur } else { 0 },
        }
    }
    (0..n)
        .map(|i| match raw[i] {
            Some(v) => v,
            None => before[i].min(after[i]),
        })
        .collect()
}

/// インデントガイドを引く桁を行ごとに返す。
///
/// **契約**: 戻り値は行数ぶんの要素を持ち、`v[i].0 == i` (0 始まりの行番号)。
/// `v[i].1` はその行で縦線を引く**桁位置**の昇順リスト (0, tab_width, ...)。
/// 空行は前後の文脈から桁を引き継ぐので、ブロックの途中の空行でも線が途切れない。
#[allow(dead_code)]
pub fn indent_guides(text: &str, tab_width: usize) -> Vec<(usize, Vec<usize>)> {
    let tw = tab_width.max(1);
    let lines: Vec<&str> = text.split('\n').collect();
    effective_indents(&lines, tw)
        .into_iter()
        .enumerate()
        .map(|(i, ind)| {
            let mut cols = Vec::new();
            let mut c = 0usize;
            while c < ind {
                cols.push(c);
                c += tw;
            }
            (i, cols)
        })
        .collect()
}

/// 強調表示するガイド (キャレットを含むブロック)。VS Code の
/// `editor.guides.highlightActiveIndentation` 相当。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ActiveGuide {
    /// 強調する縦線の桁位置。
    pub column: usize,
    /// 強調する範囲の最初の行 (ブロックの中身の先頭)。
    pub start_line: usize,
    /// 強調する範囲の最後の行。
    pub end_line: usize,
}

/// キャレット行から見た「今いるブロック」のガイドを求める。
///
/// キャレットがブロックを**開く行**にいるとき (次の行がより深いとき) は、
/// その開いたブロックのガイドを返す。深さ 0 で何も囲んでいなければ `None`。
#[allow(dead_code)]
pub fn active_guide(text: &str, tab_width: usize, caret_line: usize) -> Option<ActiveGuide> {
    let tw = tab_width.max(1);
    let lines: Vec<&str> = text.split('\n').collect();
    let n = lines.len();
    if caret_line >= n {
        return None;
    }
    let eff = effective_indents(&lines, tw);
    let raw: Vec<Option<usize>> = lines.iter().map(|l| visual_indent(l, tw)).collect();
    let own = eff[caret_line];
    let opens = raw[(caret_line + 1).min(n)..]
        .iter()
        .flatten()
        .next()
        .is_some_and(|&v| v > own);
    let column = if opens {
        (own / tw) * tw
    } else if own == 0 {
        return None;
    } else {
        ((own - 1) / tw) * tw
    };
    let anchor = if eff[caret_line] > column {
        caret_line
    } else {
        caret_line + 1
    };
    if anchor >= n || eff[anchor] <= column {
        return None;
    }
    let mut s = anchor;
    while s > 0 && eff[s - 1] > column {
        s -= 1;
    }
    let mut e = anchor;
    while e + 1 < n && eff[e + 1] > column {
        e += 1;
    }
    Some(ActiveGuide {
        column,
        start_line: s,
        end_line: e,
    })
}

/// 画面上端に貼り付けておく「今いる文脈」の行 (VS Code のスティッキースクロール)。
///
/// **契約**: `top_visible_line` (0 始まり) より上にあって、その行をまだ
/// 囲んでいる範囲のヘッダ行を、**外側から順に**返す。`max_rows` を超える
/// ぶんは内側 (深い方) から落とす。戻り値は `(行番号, 行の本文)` で、
/// 行末の空白は落としてある。コメントの塊はヘッダにしない。
#[allow(dead_code)]
pub fn sticky_headers(
    text: &str,
    lang: &str,
    top_visible_line: usize,
    max_rows: usize,
) -> Vec<(usize, String)> {
    if max_rows == 0 || top_visible_line == 0 {
        return Vec::new();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let spec = lang_spec(lang);
    let mut heads: Vec<usize> = fold_ranges(text, lang)
        .into_iter()
        .filter(|r| r.kind != FoldKind::Comment)
        .filter(|r| r.start_line < top_visible_line && top_visible_line <= r.end_line)
        .map(|r| header_line(&lines, r.start_line, spec))
        .collect();
    heads.sort_unstable();
    heads.dedup();
    heads.retain(|&l| lines.get(l).is_some_and(|s| !s.trim().is_empty()));
    heads.truncate(max_rows);
    heads
        .into_iter()
        .map(|l| (l, lines[l].trim_end().to_string()))
        .collect()
}

/// 開き括弧だけの行 (Allman スタイル) は、その 1 行上を見出しとして使う。
fn header_line(lines: &[&str], start: usize, spec: &LangSpec) -> usize {
    let Some(cur) = lines.get(start) else {
        return start;
    };
    let t = cur.trim();
    let only_brackets = !t.is_empty() && t.chars().all(|c| spec.brackets.iter().any(|p| p.0 == c));
    if !only_brackets {
        return start;
    }
    let mut i = start;
    while i > 0 {
        i -= 1;
        if lines.get(i).is_some_and(|s| !s.trim().is_empty()) {
            return i;
        }
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// SyntaxSet / ThemeSet のロードは重いので、テスト全体で 1 個だけ作って共有する。
    /// (syntect の SyntaxSet は Send + Sync なので static に置ける)
    fn hl() -> &'static Highlighter {
        static HL: OnceLock<Highlighter> = OnceLock::new();
        HL.get_or_init(Highlighter::new)
    }

    /// 実プロダクトでも使われているテーマ名 (theme.rs のダークテーマ既定値)。
    const THEME: &str = "base16-ocean.dark";

    fn font() -> FontId {
        FontId::monospace(12.0)
    }

    /// どのテーマ色とも被りにくい番兵色。これが出たら「ハイライトせず素通し」の証拠。
    fn fallback() -> Color32 {
        Color32::from_rgb(1, 2, 3)
    }

    fn job_of(text: &str, lang: &str) -> LayoutJob {
        hl().layout_job(text, lang, THEME, font(), fallback())
    }

    /// スパン列の健全性: 入力文字列を完全に復元し、単調・非重複・境界内・
    /// かつ char 境界を割っていないこと。マルチバイト崩れの検出器。
    fn assert_spans_ok(job: &LayoutJob, text: &str) {
        assert_eq!(job.text, text, "layout job must reproduce the input text");

        let mut prev_end = 0usize;
        for (i, s) in job.sections.iter().enumerate() {
            assert!(
                s.byte_range.start <= s.byte_range.end,
                "section {i} has an inverted range {:?}",
                s.byte_range
            );
            assert_eq!(
                s.byte_range.start, prev_end,
                "section {i} must start where the previous one ended (no gap / no overlap)"
            );
            assert!(
                s.byte_range.end <= job.text.len(),
                "section {i} range {:?} exceeds text length {}",
                s.byte_range,
                job.text.len()
            );
            assert!(
                job.text.is_char_boundary(s.byte_range.start)
                    && job.text.is_char_boundary(s.byte_range.end),
                "section {i} range {:?} splits a UTF-8 char boundary",
                s.byte_range
            );
            prev_end = s.byte_range.end;
        }

        if !text.is_empty() {
            assert_eq!(
                prev_end,
                text.len(),
                "sections must cover the whole text, ending at its length"
            );
        }
    }

    fn color_at(job: &LayoutJob, byte_idx: usize) -> Color32 {
        job.sections
            .iter()
            .find(|s| s.byte_range.start <= byte_idx && byte_idx < s.byte_range.end)
            .map(|s| s.format.color)
            .unwrap_or_else(|| panic!("no section covers byte {byte_idx}"))
    }

    /// `needle` の最初の出現位置の色を返す。
    fn color_of(job: &LayoutJob, needle: &str) -> Color32 {
        let i = job
            .text
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not found in laid out text"));
        color_at(job, i)
    }

    // ---- lang_for -------------------------------------------------------

    #[test]
    fn lang_for_resolves_known_extension() {
        assert_eq!(
            hl().lang_for(Some(Path::new("a.rs")), "fn main() {}"),
            "Rust"
        );
    }

    #[test]
    fn lang_for_unknown_extension_falls_back_to_plain_text() {
        assert_eq!(
            hl().lang_for(Some(Path::new("notes.zzqqxx")), "hello world"),
            "Plain Text"
        );
    }

    #[test]
    fn lang_for_uses_whole_file_name_when_there_is_no_extension() {
        // 拡張子なしファイル (Makefile 等) は file_name 経由で解決される分岐。
        assert_ne!(
            hl().lang_for(Some(Path::new("/proj/Makefile")), "all:\n\techo hi\n"),
            "Plain Text"
        );
    }

    #[test]
    fn lang_for_falls_back_to_first_line_when_path_is_none() {
        // シェバンによる判定 (path が無いケース)。
        assert_ne!(
            hl().lang_for(None, "#!/usr/bin/env python3\nprint(1)\n"),
            "Plain Text"
        );
    }

    #[test]
    fn lang_for_prefers_extension_over_first_line() {
        // 拡張子が勝つこと。シェバンに引きずられて Python にならない。
        assert_eq!(
            hl().lang_for(Some(Path::new("a.rs")), "#!/usr/bin/env python3\n"),
            "Rust"
        );
    }

    #[test]
    fn lang_for_handles_empty_text_without_path() {
        assert_eq!(hl().lang_for(None, ""), "Plain Text");
    }

    #[test]
    fn lang_for_handles_multibyte_first_line() {
        // 日本語だけの 1 行目で first_line 判定に入っても panic しない。
        let lang = hl().lang_for(None, "日本語のテキストです\n2行目\n");
        assert!(!lang.is_empty());
    }

    // ---- lang_for_fence -------------------------------------------------

    #[test]
    fn lang_for_fence_resolves_name_token() {
        assert_eq!(hl().lang_for_fence("rust"), "Rust");
    }

    #[test]
    fn lang_for_fence_resolves_extension_token() {
        assert_eq!(hl().lang_for_fence("py"), "Python");
    }

    #[test]
    fn lang_for_fence_empty_token_is_plain_text() {
        assert_eq!(hl().lang_for_fence(""), "Plain Text");
    }

    #[test]
    fn lang_for_fence_unknown_token_is_plain_text() {
        assert_eq!(hl().lang_for_fence("no-such-language-xyz"), "Plain Text");
    }

    // ---- トークン分類 ---------------------------------------------------

    #[test]
    fn keyword_and_function_name_get_different_colors() {
        let job = job_of("fn main() {}\n", "Rust");
        assert_ne!(
            color_of(&job, "fn"),
            color_of(&job, "main"),
            "keyword and function name must not share a color"
        );
    }

    #[test]
    fn number_literal_differs_from_identifier() {
        let job = job_of("let n = 42;\n", "Rust");
        assert_ne!(color_of(&job, "42"), color_of(&job, "n ="));
    }

    #[test]
    fn comment_and_string_get_different_colors() {
        let comment = job_of("// alpha\n", "Rust");
        let string = job_of("let s = \"alpha\";\n", "Rust");
        assert_ne!(
            color_of(&comment, "alpha"),
            color_of(&string, "alpha"),
            "a comment and a string literal must not share a color"
        );
    }

    // ---- 文字列内の誤分類 -----------------------------------------------

    #[test]
    fn keyword_inside_string_literal_is_not_colored_as_keyword() {
        let text = "let s = \"fn abc\";\n";
        let job = job_of(text, "Rust");
        let inside_fn = job.text.find("\"fn").expect("quote") + 1;
        let inside_abc = job.text.find("abc").expect("abc");

        assert_eq!(
            color_at(&job, inside_fn),
            color_at(&job, inside_abc),
            "`fn` inside a string must be colored like the rest of the string"
        );
        assert_ne!(
            color_at(&job, inside_fn),
            color_of(&job, "let"),
            "`fn` inside a string must not be colored as a keyword"
        );
    }

    #[test]
    fn comment_marker_inside_string_does_not_start_a_comment() {
        let with_marker = job_of("let s = \"// not a comment\";\nlet n = 1;\n", "Rust");
        let baseline = job_of("let n = 1;\n", "Rust");
        assert_eq!(
            color_of(&with_marker, "1"),
            color_of(&baseline, "1"),
            "code after a string containing `//` must still be highlighted as code"
        );
    }

    #[test]
    fn block_comment_marker_inside_string_does_not_open_a_comment() {
        let with_marker = job_of("let s = \"/* open\";\nlet n = 7;\n", "Rust");
        let baseline = job_of("let n = 7;\n", "Rust");
        assert_eq!(color_of(&with_marker, "7"), color_of(&baseline, "7"));
    }

    // ---- 未終端トークン -------------------------------------------------

    #[test]
    fn unterminated_string_is_laid_out_without_panicking() {
        let text = "let s = \"never closed\nlet t = 2;\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn unterminated_block_comment_is_laid_out_without_panicking() {
        let text = "/* open block\nstill inside\nand still\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn unterminated_string_at_eof_without_newline_is_laid_out() {
        let text = "let s = \"dangling";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    // ---- マルチバイト ---------------------------------------------------

    #[test]
    fn japanese_comment_does_not_panic_and_preserves_text() {
        let text = "// 日本語のコメント\nfn main() {}\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn japanese_string_literal_does_not_panic_and_preserves_text() {
        let text = "let s = \"日本語\";\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn unterminated_japanese_string_does_not_panic() {
        // 未終端 × マルチバイトの合わせ技。byte index スライスが char 境界を
        // 割るなら、ここが最初に落ちる。
        let text = "let s = \"日本語のまま閉じない\nlet t = 3;\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn emoji_and_combining_characters_are_preserved() {
        let text = "let s = \"🎌 とれ́ま\"; // 絵文字\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn japanese_survives_the_plain_text_fallback_path() {
        let text = "日本語のプレーンテキスト\n2行目\n";
        let job = job_of(text, "Plain Text");
        assert_spans_ok(&job, text);
    }

    // ---- 空・境界・フォールバック ---------------------------------------

    #[test]
    fn empty_text_produces_no_visible_content() {
        let job = job_of("", "Rust");
        assert_eq!(job.text, "");
    }

    #[test]
    fn empty_text_in_plain_mode_produces_no_visible_content() {
        let job = job_of("", "Plain Text");
        assert_eq!(job.text, "");
    }

    #[test]
    fn blank_and_whitespace_only_lines_are_preserved() {
        let text = "fn a() {}\n\n   \n\nfn b() {}\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn text_without_trailing_newline_is_fully_covered() {
        let text = "fn main() { let x = 1; }";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn crlf_line_endings_are_preserved_verbatim() {
        let text = "fn a() {}\r\n// コメント\r\n";
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    #[test]
    fn unknown_language_uses_the_fallback_color_in_one_section() {
        let text = "fn main() {}\n";
        let job = job_of(text, "No Such Language 12345");
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.color, fallback());
        assert_eq!(job.text, text);
    }

    #[test]
    fn unknown_theme_falls_back_to_unhighlighted_text() {
        let text = "fn main() {}\n";
        let job = hl().layout_job(text, "Rust", "no-such-theme-xyz", font(), fallback());
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.color, fallback());
        assert_eq!(job.text, text);
    }

    #[test]
    fn plain_text_language_skips_highlighting() {
        let text = "fn main() {}\n";
        let job = job_of(text, "Plain Text");
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.color, fallback());
    }

    #[test]
    fn oversized_text_is_highlighted_at_the_visible_window() {
        // 上限超えの文書は「諦めて素の文字」ではなく、可視域だけを塗る経路へ
        // 落ちる。可視域を渡さない `layout_job` では先頭ブロックが塗られる。
        let unit = "fn main() { let x = 1; }\n";
        let text = unit.repeat(MAX_HIGHLIGHT_BYTES / unit.len() + 2);
        assert!(text.len() > MAX_HIGHLIGHT_BYTES);

        let job = job_of(&text, "Rust");
        assert_eq!(job.text, text, "本文が欠けた");
        assert_spans_ok(&job, &text);
        assert!(job.sections.len() > 1, "上限超えを塗っていない");
        // 先頭は色が付き、可視域の外はフォールバック 1 色にまとまる。
        assert_ne!(job.sections[0].format.color, fallback());
        assert_eq!(
            job.sections[job.sections.len() - 1].format.color,
            fallback()
        );
    }

    #[test]
    fn oversized_single_line_is_passed_through_without_highlighting() {
        // minify された JS のような 1 行だけ巨大なテキストでも、その行は
        // 素通し (フォールバック色) にして残りの行はハイライトを続ける。
        let long_line = format!(
            "let s = \"{}\";\n",
            "x".repeat(MAX_HIGHLIGHT_LINE_BYTES + 1)
        );
        let text = format!("fn main() {{}}\n{long_line}// tail\n");
        let job = job_of(&text, "Rust");
        assert_spans_ok(&job, &text);

        // 巨大行はフォールバック色で 1 スパンとして追加される
        let long_start = text.find("let s").expect("long line present");
        assert_eq!(color_at(&job, long_start), fallback());
        // 前後の行は通常どおりハイライトされる
        assert_ne!(color_of(&job, "fn"), fallback());
        assert_ne!(color_of(&job, "// tail"), fallback());
    }

    #[test]
    fn requested_font_is_applied_to_every_section() {
        let job = job_of("// コメント\nfn main() { let x = \"s\"; }\n", "Rust");
        assert!(job.sections.iter().all(|s| s.format.font_id == font()));
    }

    #[test]
    fn wrapping_is_disabled_so_the_editor_can_scroll_horizontally() {
        let job = job_of("fn main() {}\n", "Rust");
        assert_eq!(job.wrap.max_width, f32::INFINITY);
    }

    #[test]
    fn mixed_token_document_keeps_spans_well_formed() {
        let text = concat!(
            "// 日本語のコメント: \"fn\" や // を含む\n",
            "/* block\n",
            "   still block */\n",
            "fn main() {\n",
            "    let s = \"fn // /* 日本語 \\\" escaped\";\n",
            "    let n = 0x1F + 42.5;\n",
            "\n",
            "    println!(\"{}\", s);\n",
            "}\n",
        );
        let job = job_of(text, "Rust");
        assert_spans_ok(&job, text);
    }

    // =======================================================================
    // 構造解析 (折りたたみ / インデントガイド / スティッキー)
    // =======================================================================

    /// 折りたたみ範囲を `(開始, 終了, 種類)` の並びにして比べやすくする。
    fn folds(text: &str, lang: &str) -> Vec<(usize, usize, FoldKind)> {
        fold_ranges(text, lang)
            .into_iter()
            .map(|r| (r.start_line, r.end_line, r.kind))
            .collect()
    }

    fn starts(text: &str, lang: &str) -> Vec<usize> {
        fold_ranges(text, lang)
            .into_iter()
            .map(|r| r.start_line)
            .collect()
    }

    #[test]
    fn lang_spec_table_picks_strategy() {
        // 表引きが効いているか (名前の大文字小文字は無視)
        let cases = [
            ("Rust", FoldStrategy::Brackets),
            ("rust", FoldStrategy::Brackets),
            ("C++", FoldStrategy::Brackets),
            ("Javascript", FoldStrategy::Brackets),
            ("JSON", FoldStrategy::Brackets),
            ("Python", FoldStrategy::Indent),
            ("YAML", FoldStrategy::Indent),
            ("Markdown", FoldStrategy::Markdown),
            ("Plain Text", FoldStrategy::Indent),
            ("なにか未知の言語", FoldStrategy::Indent),
        ];
        for (lang, want) in cases {
            assert_eq!(lang_spec(lang).strategy, want, "lang={lang}");
        }
        assert_eq!(lang_spec("Python").line_comment, &["#"]);
        assert_eq!(lang_spec("Rust").line_comment, &["//"]);
        // Rust はライフタイム `'a` があるのでシングルクォートを文字列にしない
        assert!(!lang_spec("Rust").quotes.contains(&'\''));
        assert!(lang_spec("C").quotes.contains(&'\''));
    }

    #[test]
    fn fold_rust_nested_braces() {
        let text = "\
fn a() {
    if x {
        y();
    }
}
fn b() {}
";
        let f = folds(text, "Rust");
        assert!(
            f.contains(&(0, 4, FoldKind::Block)),
            "外側の fn が 0..4 で畳める: {f:?}"
        );
        assert!(
            f.contains(&(1, 3, FoldKind::Block)),
            "内側の if が 1..3 で畳める: {f:?}"
        );
        assert!(
            !starts(text, "Rust").contains(&5),
            "1 行で閉じる fn b は畳めない: {f:?}"
        );
    }

    #[test]
    fn fold_rust_block_comment_and_strings() {
        let text = "\
/* これは
   複数行の
   コメント */
fn a() {
    let s = \"} 文字列の中の括弧は数えない {\";
    // 行コメント 1
    // 行コメント 2
    s
}
";
        let f = folds(text, "Rust");
        assert!(
            f.contains(&(0, 2, FoldKind::Comment)),
            "ブロックコメントが畳める: {f:?}"
        );
        assert!(
            f.contains(&(3, 8, FoldKind::Block)),
            "文字列中の括弧に釣られず fn が 3..8: {f:?}"
        );
        assert!(
            f.contains(&(5, 6, FoldKind::Comment)),
            "連続した行コメントも畳める: {f:?}"
        );
    }

    #[test]
    fn fold_rust_broken_source_never_panics() {
        // 閉じ忘れ・入れ違い・未終端文字列でも落ちない
        for text in [
            "fn a() {\n  {\n",
            "}\n}\n{\n",
            "let s = \"未終端\nfn b() {\n}\n",
            "/* 閉じ忘れコメント\nfn c() {\n}\n",
            "",
            "\n\n\n",
        ] {
            let _ = fold_ranges(text, "Rust");
        }
    }

    #[test]
    fn fold_python_indent_and_blank_lines() {
        let text = "\
def outer():
    def inner():
        pass

    return inner

def other():
    pass
";
        let f = folds(text, "Python");
        assert!(
            f.contains(&(0, 4, FoldKind::Indent)),
            "outer は空行を跨いで 0..4 (末尾の空行は含めない): {f:?}"
        );
        assert!(
            f.contains(&(1, 2, FoldKind::Indent)),
            "inner は 1..2: {f:?}"
        );
        assert!(
            f.contains(&(6, 7, FoldKind::Indent)),
            "other は 6..7: {f:?}"
        );
    }

    #[test]
    fn fold_python_docstring_is_comment() {
        let text = "\
def f():
    \"\"\"これは
    docstring
    \"\"\"
    return 1
";
        let f = folds(text, "Python");
        assert!(
            f.contains(&(1, 3, FoldKind::Comment)),
            "docstring が畳める: {f:?}"
        );
        assert!(f.iter().any(|r| r.0 == 0), "def 自体も畳める: {f:?}");
    }

    #[test]
    fn fold_yaml_indent() {
        let text = "\
top:
  a: 1
  b:
    - x
    - y
other: 2
";
        let f = folds(text, "YAML");
        assert!(f.contains(&(0, 4, FoldKind::Indent)), "top は 0..4: {f:?}");
        assert!(f.contains(&(2, 4, FoldKind::Indent)), "b は 2..4: {f:?}");
        assert!(
            !starts(text, "YAML").contains(&5),
            "最終行は畳めない: {f:?}"
        );
    }

    #[test]
    fn fold_markdown_heading_hierarchy() {
        let text = "\
# 見出し1
本文A

## 見出し2
本文B

### 見出し3
本文C

## 見出し2b
本文D
";
        let f = folds(text, "Markdown");
        assert!(
            f.contains(&(0, 10, FoldKind::Section)),
            "# は最後まで: {f:?}"
        );
        assert!(
            f.contains(&(3, 7, FoldKind::Section)),
            "最初の ## は次の ## の直前まで: {f:?}"
        );
        assert!(f.contains(&(6, 7, FoldKind::Section)), "### は 6..7: {f:?}");
        assert!(
            f.contains(&(9, 10, FoldKind::Section)),
            "最後の ## は末尾まで: {f:?}"
        );
    }

    #[test]
    fn fold_markdown_fence_and_heading_in_fence() {
        let text = "\
# 題
```rust
# これは見出しではない
fn a() {}
```
おわり
";
        let f = folds(text, "Markdown");
        assert!(
            f.contains(&(1, 4, FoldKind::Block)),
            "コードフェンスが畳める: {f:?}"
        );
        assert_eq!(
            f.iter().filter(|r| r.2 == FoldKind::Section).count(),
            1,
            "フェンス内の # は見出しにしない: {f:?}"
        );
    }

    #[test]
    fn fold_mixed_tabs_and_spaces() {
        // タブ 1 個 = 4 桁として、空白 4 個と同じ深さに見えること
        let text = "def f():\n\tfirst\n    second\n\t\tdeep\nafter\n";
        let f = folds(text, "Python");
        assert!(
            f.contains(&(0, 3, FoldKind::Indent)),
            "タブと空白を同じ深さとして 0..3: {f:?}"
        );
        assert!(
            f.contains(&(2, 3, FoldKind::Indent)),
            "4 空白の行がタブ 2 個の行を抱える: {f:?}"
        );
        assert!(
            !f.iter().any(|r| r.0 == 1),
            "タブ 1 個と空白 4 個は同じ深さなので親子にならない: {f:?}"
        );
        // タブ幅を変えると深さの比較そのものが変わる
        let t2 = "a:\n\tb\n     c\n";
        let g = |tw: usize| -> Vec<(usize, usize)> {
            fold_ranges_with(t2, "YAML", tw)
                .into_iter()
                .map(|r| (r.start_line, r.end_line))
                .collect()
        };
        assert!(
            g(4).contains(&(1, 2)),
            "タブ幅 4 ならタブ行 (4 桁) より 5 空白の行が深い: {:?}",
            g(4)
        );
        assert!(
            !g(8).contains(&(1, 2)),
            "タブ幅 8 ならタブ行 (8 桁) の方が深い: {:?}",
            g(8)
        );
    }

    #[test]
    fn fold_huge_file_is_skipped() {
        let text = "a\n".repeat(FOLD_MAX_LINES + 1);
        assert!(
            fold_ranges(&text, "Plain Text").is_empty(),
            "行数上限を超えたら計算しない"
        );
    }

    #[test]
    fn fold_ranges_are_sorted_and_unique_by_start() {
        let text = "\
fn a() {
    /* c
       c */
    if x {
    }
}
";
        let r = fold_ranges(text, "Rust");
        for w in r.windows(2) {
            assert!(
                w[0].start_line < w[1].start_line,
                "開始行が昇順かつ一意: {r:?}"
            );
        }
        for x in &r {
            assert!(x.end_line > x.start_line, "空の範囲は無い: {x:?}");
        }
    }

    // ---------------- インデントガイド ----------------

    fn guide_cols(text: &str, tw: usize) -> Vec<Vec<usize>> {
        indent_guides(text, tw)
            .into_iter()
            .enumerate()
            .map(|(i, (l, c))| {
                assert_eq!(i, l, "行番号は添字と一致する");
                c
            })
            .collect()
    }

    #[test]
    fn indent_guides_table() {
        // (説明, 本文, タブ幅, 各行の期待ガイド桁)
        let cases: &[(&str, &str, usize, &[&[usize]])] = &[
            (
                "空白 4 のネスト",
                "a\n    b\n        c\n",
                4,
                &[&[], &[0], &[0, 4], &[]],
            ),
            (
                "タブのネスト",
                "a\n\tb\n\t\tc\n",
                4,
                &[&[], &[0], &[0, 4], &[]],
            ),
            (
                "タブと空白の混在 (同じ深さに見える)",
                "a\n\tb\n    c\n",
                4,
                &[&[], &[0], &[0], &[]],
            ),
            ("タブ幅 2", "a\n  b\n    c\n", 2, &[&[], &[0], &[0, 2], &[]]),
            (
                "深いネスト",
                "a\n    b\n        c\n            d\n",
                4,
                &[&[], &[0], &[0, 4], &[0, 4, 8], &[]],
            ),
        ];
        for (name, text, tw, want) in cases {
            let got = guide_cols(text, *tw);
            let want: Vec<Vec<usize>> = want.iter().map(|v| v.to_vec()).collect();
            assert_eq!(&got, &want, "{name}");
        }
    }

    #[test]
    fn indent_guides_blank_line_inherits_context() {
        // ブロックの途中の空行は線が途切れず、ブロックの切れ目では浅い方を継ぐ
        let text = "def f():\n    a\n\n    b\ndef g():\n    c\n";
        let g = guide_cols(text, 4);
        assert_eq!(g[2], vec![0], "ブロック中の空行はガイドを保つ");
        let text2 = "def f():\n    if x:\n        a\n\n    b\n";
        let g2 = guide_cols(text2, 4);
        assert_eq!(
            g2[3],
            vec![0],
            "ブロックの切れ目の空行は浅い方 (min) を継ぐ"
        );
        let text3 = "\n\n    a\n";
        let g3 = guide_cols(text3, 4);
        assert_eq!(g3[0], Vec::<usize>::new(), "先頭の空行はガイド無し");
    }

    #[test]
    fn active_guide_selection() {
        let text = "\
def f():
    if x:
        a
        b
    c
d
";
        // 中身の行にいるときは、その行を囲むいちばん内側のブロック
        let g = active_guide(text, 4, 2).expect("2 行目は if の中");
        assert_eq!((g.column, g.start_line, g.end_line), (4, 2, 3));
        // ブロックを開く行にいるときは、その開いたブロック
        let g = active_guide(text, 4, 1).expect("1 行目は if 自身");
        assert_eq!((g.column, g.start_line, g.end_line), (4, 2, 3));
        let g = active_guide(text, 4, 0).expect("0 行目は def 自身");
        assert_eq!((g.column, g.start_line, g.end_line), (0, 1, 4));
        // def の中の浅い行
        let g = active_guide(text, 4, 4).expect("4 行目は def の直下");
        assert_eq!((g.column, g.start_line, g.end_line), (0, 1, 4));
        // トップレベルで何も囲んでいない行
        assert!(active_guide(text, 4, 5).is_none(), "深さ 0 は None");
        assert!(active_guide(text, 4, 999).is_none(), "範囲外は None");
    }

    // ---------------- スティッキースクロール ----------------

    const NESTED: &str = "\
mod m {
    fn outer() {
        fn inner() {
            let a = 1;
            let b = 2;
        }
    }
}
fn tail() {
    let c = 3;
}
";

    #[test]
    fn sticky_headers_at_scroll_positions() {
        let heads = |top: usize, max: usize| -> Vec<(usize, String)> {
            sticky_headers(NESTED, "Rust", top, max)
                .into_iter()
                .map(|(l, s)| (l, s.trim().to_string()))
                .collect()
        };
        assert!(heads(0, 5).is_empty(), "先頭では貼るものが無い");
        assert_eq!(
            heads(3, 5),
            vec![
                (0, "mod m {".to_string()),
                (1, "fn outer() {".to_string()),
                (2, "fn inner() {".to_string()),
            ],
            "入れ子の中では外側から順に 3 段"
        );
        assert_eq!(
            heads(3, 2),
            vec![(0, "mod m {".to_string()), (1, "fn outer() {".to_string())],
            "max_rows を超えたら内側から落とす"
        );
        assert!(heads(3, 0).is_empty(), "max_rows=0 は空");
        assert_eq!(
            heads(9, 5),
            vec![(8, "fn tail() {".to_string())],
            "別の関数に入れば貼るものも入れ替わる"
        );
        assert!(
            heads(11, 5).is_empty(),
            "EOF より後ろでは貼るものが無い: {:?}",
            heads(11, 5)
        );
    }

    #[test]
    fn sticky_headers_markdown_uses_headings() {
        let text = "\
# 章
## 節
本文1
本文2
## 節2
本文3
";
        let got: Vec<_> = sticky_headers(text, "Markdown", 3, 5)
            .into_iter()
            .map(|(l, s)| (l, s))
            .collect();
        assert_eq!(
            got,
            vec![(0, "# 章".to_string()), (1, "## 節".to_string())],
            "見出しの階層がそのまま文脈になる"
        );
    }

    #[test]
    fn sticky_headers_skip_comment_blocks_and_allman_brace() {
        // Allman スタイル (開き括弧だけの行) は 1 行上を見出しにする
        let text = "\
/* 長い
   コメント */
int main(int argc)
{
    int a = 1;
    int b = 2;
}
";
        let got: Vec<_> = sticky_headers(text, "C", 5, 5)
            .into_iter()
            .map(|(l, s)| (l, s))
            .collect();
        assert_eq!(
            got,
            vec![(2, "int main(int argc)".to_string())],
            "コメントは貼らず、`{{` の行ではなく宣言行を貼る"
        );
    }

    // ───────────────── 虹色括弧 (bracket pair colorization) ─────────────────

    /// `(バイト位置, 深さ, 相手なしか)` の並びへ落として比較しやすくする。
    fn hits(text: &str, lang: &str) -> Vec<(usize, usize, bool)> {
        let v = bracket_pairs(text, lang);
        // 不変条件: 昇順・1 バイトの ASCII・必ず文字境界
        let mut prev = None;
        for h in &v {
            assert!(
                prev.map(|p| p < h.byte).unwrap_or(true),
                "昇順でない: {v:?}"
            );
            prev = Some(h.byte);
            assert!(
                text.is_char_boundary(h.byte) && text.is_char_boundary(h.byte + 1),
                "多バイト文字を割った: {h:?} in {text:?}"
            );
        }
        v.iter().map(|h| (h.byte, h.depth, h.unmatched)).collect()
    }

    /// 入れ子の深さが 0 から順に付く。
    #[test]
    fn 括弧の深さが入れ子の順に付く() {
        //             0123456789...
        let text = "fn a() { b[0] }";
        assert_eq!(
            hits(text, "Rust"),
            vec![
                (4, 0, false),  // (
                (5, 0, false),  // )
                (7, 0, false),  // {
                (10, 1, false), // [
                (12, 1, false), // ]
                (14, 0, false), // }
            ]
        );
        // 深さ N まで積み上がる
        let deep = "((((()))))";
        let got = hits(deep, "Rust");
        assert_eq!(got.len(), 10);
        for (i, h) in got.iter().take(5).enumerate() {
            assert_eq!(h.1, i, "開きの深さ");
        }
        for (i, h) in got.iter().skip(5).enumerate() {
            assert_eq!(h.1, 4 - i, "閉じは相手と同じ深さ");
        }
        assert!(got.iter().all(|h| !h.2), "全部対応が取れている");
    }

    /// 対応の取れない括弧には `unmatched` が立つ。
    #[test]
    fn 対応の取れない括弧に印が立つ() {
        assert_eq!(hits("(", "Rust"), vec![(0, 0, true)]);
        assert_eq!(hits(")", "Rust"), vec![(0, 0, true)]);
        // 入れ違い: `(` は捨てられ、`]` は相手なし
        assert_eq!(
            hits("[(]", "Rust"),
            vec![(0, 0, false), (1, 1, true), (2, 0, false)]
        );
        // 閉じ忘れ
        assert_eq!(hits("{ a", "Rust"), vec![(0, 0, true)]);
        // 空の本文
        assert!(hits("", "Rust").is_empty());
    }

    /// **文字列リテラルの中の括弧は数えない。**
    #[test]
    fn 文字列の中の括弧を数えない() {
        // 中の `(` `{` は無視され、外側の () だけが残る
        let text = "f(\"a(b{c\")";
        assert_eq!(hits(text, "Rust"), vec![(1, 0, false), (9, 0, false)]);
        // エスケープされた引用符で文字列が閉じたことにならない
        let text = "f(\"\\\"(\")";
        assert_eq!(hits(text, "Rust"), vec![(1, 0, false), (7, 0, false)]);
    }

    /// **コメントの中の括弧は数えない。**
    #[test]
    fn コメントの中の括弧を数えない() {
        // 行コメント
        let text = "a(); // ) } ]\nb();";
        let got = hits(text, "Rust");
        assert_eq!(
            got,
            vec![(1, 0, false), (2, 0, false), (15, 0, false), (16, 0, false)]
        );
        // ブロックコメント (行を跨いでも)
        let text = "/* ( */ x() /*\n{ */ y()";
        let got = hits(text, "Rust");
        assert_eq!(
            got.iter().map(|h| h.0).collect::<Vec<_>>(),
            vec![9, 10, 21, 22]
        );
        assert!(got.iter().all(|h| !h.2));
    }

    /// CJK を含む行でもバイト境界を割らない (`hits` の中で検査済み)。
    #[test]
    fn cjkを含む行でもバイト境界を割らない() {
        //          あ  (  い  )  う  [  え  ]
        // バイト:   0-2 3  4-6 7  8-10 11 12-14 15
        let text = "あ(い)う[え]";
        let got = hits(text, "Rust");
        assert_eq!(
            got,
            vec![(3, 0, false), (7, 0, false), (11, 0, false), (15, 0, false)]
        );
        // 絵文字 (4 バイト) を跨いでも同じ
        let text = "🎉(🎉)";
        assert_eq!(hits(text, "Rust"), vec![(4, 0, false), (9, 0, false)]);
    }

    /// 巨大ファイルでは走らせない (強調表示自体を止めているため)。
    #[test]
    fn 巨大ファイルでは括弧を走査しない() {
        let big = "()".repeat(MAX_HIGHLIGHT_BYTES);
        assert!(bracket_pairs(&big, "Rust").is_empty());
    }

    /// [`colorize_brackets`] は本文を動かさず、色だけを差し替える。
    #[test]
    fn 括弧の色分けは本文を動かさない() {
        let text = "a(b[c]d)e";
        let font = FontId::monospace(12.0);
        let base = Color32::WHITE;
        let mut job = LayoutJob::default();
        job.append(
            text,
            0.0,
            TextFormat {
                font_id: font.clone(),
                color: base,
                ..Default::default()
            },
        );
        let cols = [Color32::RED, Color32::GREEN];
        let err = Color32::BLUE;
        let out = colorize_brackets(job, &bracket_pairs(text, "Rust"), &cols, err);
        assert_eq!(out.text, text, "本文は 1 バイトも変わらない");
        // 節はバイト範囲が連続していて、本文全体を覆う
        let mut at = 0usize;
        for s in &out.sections {
            assert_eq!(
                s.byte_range.start, at,
                "節が連続していない: {:?}",
                out.sections
            );
            at = s.byte_range.end;
        }
        assert_eq!(at, text.len(), "本文の末尾まで覆っていない");
        // 括弧の位置の色が深さどおり ( ( → cols[0], [ → cols[1] )
        let color_at = |b: usize| {
            out.sections
                .iter()
                .find(|s| s.byte_range.start <= b && b < s.byte_range.end)
                .map(|s| s.format.color)
                .unwrap()
        };
        assert_eq!(color_at(1), cols[0], "( は深さ 0");
        assert_eq!(color_at(7), cols[0], ") は深さ 0");
        assert_eq!(color_at(3), cols[1], "[ は深さ 1");
        assert_eq!(color_at(5), cols[1], "] は深さ 1");
        assert_eq!(color_at(0), base, "括弧以外は元の色のまま");

        // 相手のいない括弧はエラー色
        let text = "(";
        let mut job = LayoutJob::default();
        job.append(
            text,
            0.0,
            TextFormat {
                font_id: font,
                color: base,
                ..Default::default()
            },
        );
        let out = colorize_brackets(job, &bracket_pairs(text, "Rust"), &cols, err);
        assert_eq!(out.sections[0].format.color, err);

        // 括弧が無ければ節はそのまま (無駄に割らない)
        let mut job = LayoutJob::default();
        job.append("abc", 0.0, TextFormat::default());
        let n = job.sections.len();
        let out = colorize_brackets(job, &[], &cols, err);
        assert_eq!(out.sections.len(), n);
    }
}

// ===========================================================================
// 同梱シンタックスパックとの結合テスト
//
// 「プラグインを入れたのに .ts が白いまま」を防ぐ番人。
// 実際に出荷するデータ (assets/plugins/syntax-pack) を読み、
// Highlighter 越しに言語判定と着色まで通す。
// ===========================================================================
#[cfg(test)]
mod pack_integration {
    use super::*;

    /// 同梱パックを積んだ専用インスタンス (`shared()` を汚さない)。
    fn hl() -> Highlighter {
        let h = Highlighter::new();
        h.set_grammars(crate::grammar::bundled_pack::load());
        h
    }

    #[test]
    fn 同梱パックで主要言語が拡張子から解決される() {
        let h = hl();
        let cases: &[(&str, &str)] = &[
            ("a/b/x.ts", "TypeScript"),
            // 拡張セットは TSX を専用構文で塗る (JSX を HTML 扱いしない)
            ("x.tsx", "TypeScriptReact"),
            ("x.mts", "TypeScript"),
            ("x.kt", "Kotlin"),
            ("x.kts", "Kotlin"),
            ("x.swift", "Swift"),
            ("x.dart", "Dart"),
            ("x.zig", "Zig"),
            ("x.ex", "Elixir"),
            ("x.jl", "Julia"),
            ("x.nim", "Nim"),
            ("x.sol", "Solidity"),
            ("x.gql", "GraphQL"),
            ("x.scss", "SCSS"),
            ("x.less", "Less"),
            ("x.ps1", "PowerShell"),
            ("x.proto", "Protocol Buffer"),
            ("x.nix", "Nix"),
            ("x.tf", "Terraform"),
            ("Cargo.toml", "TOML"),
            ("Dockerfile", "Dockerfile"),
            ("CMakeLists.txt", "CMake"),
            (".env", "DotENV"),
            (".gitignore", "Git Ignore"),
            ("justfile", "Just"),
            // パックでは HTML 扱いだったが、拡張セットには専用構文がある
            ("x.vue", "Vue Component"),
            ("x.svelte", "Svelte"),
            ("x.mjs", "JavaScript (Babel)"),
            // 拡張セットに JSON5 は無いのでパックが埋める
            ("x.json5", "JSON"),
            ("x.vhd", "VHDL"),
            ("x.sv", "SystemVerilog"),
            ("x.wgsl", "WGSL"),
            ("x.fs", "F#"),
            ("x.hx", "Haxe"),
            ("x.elm", "Elm"),
            ("x.rkt", "Racket"),
            ("x.vim", "VimL"),
            ("x.awk", "AWK"),
            // Starlark は Python の方言。拡張セットの本物の Python 構文の方が
            // パックの近似より精度が高いので、そちらへ寄せる。
            ("x.bzl", "Python"),
            ("x.bicep", "Bicep"),
            // 今回埋めた 7 言語。`.v` は Verilog のまま (奪わない) で、
            // V は曖昧でない拡張子とマニフェスト名から引く。
            ("x.cue", "CUE"),
            ("x.kdl", "KDL"),
            ("x.dhall", "Dhall"),
            ("x.cob", "COBOL"),
            ("x.cbl", "COBOL"),
            ("x.apex", "Apex"),
            ("x.trigger", "Apex"),
            ("x.sed", "sed"),
            ("x.vv", "V"),
            // `.vsh` は GLSL (頂点シェーダ) のまま
            ("x.vsh", "GLSL"),
            ("v.mod", "V"),
            ("x.v", "Verilog"),
        ];
        for (path, want) in cases {
            let got = h.lang_for(Some(Path::new(path)), "");
            assert_eq!(&got, want, "{path} の言語判定");
        }
    }

    #[test]
    fn 既存のsyntect言語は影響を受けない() {
        let h = hl();
        let cases: &[(&str, &str)] = &[
            ("x.rs", "Rust"),
            ("x.py", "Python"),
            ("x.go", "Go"),
            ("x.c", "C"),
            ("x.java", "Java"),
            ("x.rb", "Ruby"),
            ("x.php", "PHP"),
            ("x.html", "HTML"),
            ("x.css", "CSS"),
            ("x.json", "JSON"),
            ("x.md", "Markdown"),
            ("x.yaml", "YAML"),
            ("x.sql", "SQL"),
            // 拡張セットの既定は Babel 版 (JSX と最近の構文まで読める)
            ("x.js", "JavaScript (Babel)"),
            ("x.lua", "Lua"),
        ];
        for (path, want) in cases {
            assert_eq!(&h.lang_for(Some(Path::new(path)), ""), want, "{path}");
        }
    }

    #[test]
    fn フェンスの言語トークンも引ける() {
        let h = hl();
        for (tok, want) in [
            ("ts", "TypeScript"),
            ("typescript", "TypeScript"),
            ("kotlin", "Kotlin"),
            ("dockerfile", "Dockerfile"),
            ("toml", "TOML"),
            ("hcl", "Terraform"),
            ("rust", "Rust"),
            ("python", "Python"),
        ] {
            assert_eq!(h.lang_for_fence(tok), want, "```{tok}");
        }
        assert_eq!(h.lang_for_fence("しらない言語"), "Plain Text");
    }

    #[test]
    fn 追加言語に実際に色が付く() {
        let h = hl();
        let src = "// コメント\nexport const x: number = 42;\nconsole.log(\"hi\");\n";
        let job = h.layout_job(
            src,
            "TypeScript",
            "base16-ocean.dark",
            FontId::monospace(12.0),
            Color32::WHITE,
        );
        let mut colors: Vec<[u8; 4]> = job
            .sections
            .iter()
            .map(|s| s.format.color.to_array())
            .collect();
        colors.sort();
        colors.dedup();
        assert!(
            colors.len() >= 4,
            "TypeScript が単色で塗られている (色数 {})",
            colors.len()
        );
        // 本文が欠けていないこと (連結すると元に戻る)
        assert_eq!(job.text, src);
    }

    #[test]
    fn 同梱パックの全言語が着色経路を通る() {
        let h = hl();
        let src = "a = 1 // b\n#c\n\"s\"\n";
        for name in crate::grammar::bundled_pack::load().names() {
            let job = h.layout_job(
                src,
                name,
                "base16-ocean.dark",
                FontId::monospace(12.0),
                Color32::WHITE,
            );
            assert_eq!(job.text, src, "{name}: 本文が欠けた");
            assert!(job.sections.len() > 1, "{name}: 素通し (色が付いていない)");
        }
    }

    #[test]
    fn 追加言語の折りたたみとコメント記号が登録される() {
        let h = hl();
        // 組み込みの表に無い言語 (Elixir / Nix) が引けること
        assert_eq!(lang_spec("Elixir").line_comment, &["#"]);
        assert_eq!(lang_spec("Elixir").strategy, FoldStrategy::Indent);
        assert_eq!(lang_spec("Zig").strategy, FoldStrategy::Brackets);
        assert_eq!(dynamic_line_comment("Nix"), Some("#"));
        assert_eq!(dynamic_line_comment("Elixir"), Some("#"));
        assert_eq!(dynamic_line_comment("Zig"), Some("//"));
        // 知らない言語は None のまま
        assert_eq!(dynamic_line_comment("まだ無い言語"), None);
        // 使わない警告避け (hl() を通してパックを登録するのが前提)
        assert!(h.extra_lang_count() >= 50);
    }

    /// 起動と描画に載る費用の番人。
    ///
    /// **読み込みは時間ではなく「読むバイト数」で測る。** 以前は
    /// `load < 600ms` と絶対時間で線を引いていたが、並列ビルドの下で
    /// **727ms を記録して落ちた** — 実装は 1 バイトも変わっていないのに。
    /// CLAUDE.md の「絶対時間で性能テストの線を引かない。必ず嘘をつく」の実例で、
    /// 名指しで挙げられている直し方 (構文セットの大きさを測る) がこれ。
    ///
    /// 費用を決めているのは**読む量**なので、そこを見れば
    /// 「巨大な定義を足して起動を重くした」は負荷に関係なく捕まる。
    /// 塗りのほうは下で**比と最小値**を使う (絶対値は debug/release で 2 桁違う)。
    #[test]
    fn 読み込みと着色が遅くなっていない() {
        // 同梱パックの実体を数える (実測 2026-08-13: 6 ファイル / 62,034 バイト)。
        // 上限は実測の約 4 倍 — 言語を足す余地は残しつつ、桁が変わったら落ちる。
        const MAX_PACK_BYTES: u64 = 256 * 1024;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/plugins/syntax-pack/syntaxes");
        let (mut files, mut bytes) = (0u32, 0u64);
        for e in std::fs::read_dir(&dir)
            .expect("同梱パックのディレクトリ")
            .flatten()
        {
            if let Ok(m) = e.metadata() {
                if m.is_file() {
                    files += 1;
                    bytes += m.len();
                }
            }
        }
        assert!(files > 0, "同梱パックが空 (数え方が壊れている)");
        assert!(
            bytes <= MAX_PACK_BYTES,
            "同梱パックが太った: {files} ファイル / {bytes} バイト (上限 {MAX_PACK_BYTES})"
        );

        let set = crate::grammar::bundled_pack::load();

        let h = Highlighter::new();
        h.set_grammars(set);

        // 初回の塗りと再描画の塗りを N 回ずつ測り、**それぞれの最小値**で比べる。
        //
        // 1 回ずつしか測らないと、比を取る 2 つの計測が**別々の負荷の下**で
        // 行われるため、フルスイート実行のような混雑した環境で
        // 「再描画の方が初回より遅い」という物理的にありえない結果が出て落ちる
        // (実測: 初回 4.28s / 再描画 4.93s)。最小値は負荷スパイクを拾わないので、
        // ノイズが除かれるぶん**本当に遅くなったときだけ落ちる**。
        const N: usize = 3;
        let mut paints: Vec<std::time::Duration> = Vec::with_capacity(N);
        let mut repaints: Vec<std::time::Duration> = Vec::with_capacity(N);

        for i in 0..N {
            // 毎回「本当の初回」から測る。ここを間違えると 2 回目以降が全部
            // キャッシュヒットになり、**テストが何も検査しなくなる**。
            // 外すべきキャッシュは 2 つある:
            //  1. `Highlighter::cache` — 本文込みのキー。本文に i を混ぜて外す
            //     (同じ `Highlighter` を使い回すのは、行キャッシュのキー
            //      `style_key` にインスタンスのアドレスが入っており、作り直すと
            //      同じ番地に載って前回の行キャッシュを拾いうるため)
            //  2. `DOC_CACHE` — 行キャッシュ。空にして全行解析へ落とす
            let src = format!("// {i}\n")
                + &"// c\nexport const x: number = foo(1) + \"s\";\n".repeat(1000);
            DOC_CACHE.with(|c| *c.borrow_mut() = None);
            let t = std::time::Instant::now();
            let job = h.layout_job(
                &src,
                "TypeScript",
                "base16-ocean.dark",
                FontId::monospace(12.0),
                Color32::WHITE,
            );
            paints.push(t.elapsed());
            assert_eq!(job.text, src);

            // 1 行だけ書き換えたときの塗り直し。直前の初回が残した行キャッシュが
            // 効いていれば、変更点の周辺だけを解析し直すので初回よりはっきり速い。
            // **比**で見るのは、debug/release とマシンの負荷で絶対値が
            // 2 桁変わるため (実測: 同じコードで debug 5.3s / release 0.2s)。
            let edited = src.replacen("export const x", "export const y", 1);
            let t = std::time::Instant::now();
            let job2 = h.layout_job(
                &edited,
                "TypeScript",
                "base16-ocean.dark",
                FontId::monospace(12.0),
                Color32::WHITE,
            );
            repaints.push(t.elapsed());
            assert_eq!(job2.text, edited);
        }

        let paint = paints.iter().copied().min().expect("N >= 1");
        let repaint = repaints.iter().copied().min().expect("N >= 1");
        assert!(
            repaint * 5 < paint,
            "1 行編集の塗り直しが差分計算になっていない \
             (初回 min {paint:?} / 全 {paints:?}, 再描画 min {repaint:?} / 全 {repaints:?})"
        );
    }

    #[test]
    fn パック無しでも従来どおり動く() {
        let h = Highlighter::new();
        assert_eq!(h.extra_lang_count(), 0);
        assert_eq!(h.lang_for(Some(Path::new("x.rs")), ""), "Rust");
        // 知らない言語は素通し (落ちない)
        let job = h.layout_job(
            "x = 1\n",
            "TypeScript",
            "base16-ocean.dark",
            FontId::monospace(12.0),
            Color32::WHITE,
        );
        assert_eq!(job.text, "x = 1\n");
    }
}

// ===========================================================================
// 差分ハイライト (行キャッシュ) の検証
//
// `highlight_doc` は「前回の結果」を再利用するため、**再利用した結果が
// 毎回ゼロから塗った結果と 1 バイトも違わない**ことが生命線になる。
// ここが崩れると「編集したら遠くの行の色が壊れる」という、再現しにくい
// 不具合になるので、編集列を実際に流して総当たりで突き合わせる。
// ===========================================================================
#[cfg(test)]
mod incremental {
    use super::*;

    fn theme() -> &'static str {
        "base16-ocean.dark"
    }

    /// 行キャッシュを空にしてから 1 回塗る (= ゼロから塗った答え)。
    fn from_scratch(h: &Highlighter, text: &str, lang: &str) -> LayoutJob {
        DOC_CACHE.with(|c| *c.borrow_mut() = None);
        h.layout_job(text, lang, theme(), FontId::monospace(12.0), Color32::WHITE)
    }

    /// 直前の結果を残したまま塗る (= 差分計算した答え)。
    fn incremental(h: &Highlighter, text: &str, lang: &str) -> LayoutJob {
        h.layout_job(text, lang, theme(), FontId::monospace(12.0), Color32::WHITE)
    }

    fn same(a: &LayoutJob, b: &LayoutJob) -> bool {
        a.text == b.text
            && a.sections.len() == b.sections.len()
            && a.sections.iter().zip(&b.sections).all(|(x, y)| {
                x.byte_range == y.byte_range
                    && x.format.color == y.format.color
                    && x.format.italics == y.format.italics
            })
    }

    /// 8KB 以上でないと行キャッシュに載らないので、嵩を出すための下敷き。
    fn padded(head: &str, body_lines: usize) -> String {
        let mut s = String::from(head);
        for i in 0..body_lines {
            s.push_str(&format!("let v{i} = {i}; // 埋め草\n"));
        }
        s
    }

    #[test]
    fn 差分計算はゼロから塗った結果と一致する() {
        let h = Highlighter::new();
        let base = padded("// 先頭\n", 600);
        let lines: Vec<&str> = base.split_inclusive('\n').collect();
        // 先頭 / 中間 / 末尾 / チェックポイント境界のそれぞれを編集する
        let targets = [
            0usize,
            1,
            255,
            256,
            257,
            300,
            lines.len() / 2,
            lines.len() - 1,
        ];
        // まず 1 回塗って行キャッシュを作る
        let _ = from_scratch(&h, &base, "Rust");
        for t in targets {
            for edit in [
                "let z = \"文字列\";\n",    // 置換
                "/* 開いたまま\n",          // 行を跨ぐコメントを開く
                "*/ let w = 1;\n",          // 閉じる
                "",                         // 行の削除
                "let a = 1;\nlet b = 2;\n", // 行の挿入
            ] {
                let mut v = lines.clone();
                if edit.is_empty() {
                    v.remove(t);
                } else {
                    v[t] = edit;
                }
                let text: String = v.concat();
                let inc = incremental(&h, &text, "Rust");
                let full = from_scratch(&h, &text, "Rust");
                assert!(
                    same(&inc, &full),
                    "{t} 行目を {edit:?} に変えたときの差分計算がズレた"
                );
                // 次の編集のために、いまの本文で行キャッシュを作り直す
                let _ = incremental(&h, &text, "Rust");
            }
        }
    }

    #[test]
    fn 行を跨ぐコメントを開いたり閉じたりしても一致する() {
        let h = Highlighter::new();
        let body = padded("", 800);
        let opened = format!("/* ここから\n{body}");
        let closed = format!("/* ここから\n{body}*/\n");
        let _ = from_scratch(&h, &opened, "Rust");
        // 末尾に閉じ記号を足す = 末尾一致が効かない編集
        let inc = incremental(&h, &closed, "Rust");
        let full = from_scratch(&h, &closed, "Rust");
        assert!(same(&inc, &full), "コメントを閉じたときにズレた");
        // 逆向き (閉じ記号を消す)
        let _ = incremental(&h, &closed, "Rust");
        let inc = incremental(&h, &opened, "Rust");
        let full = from_scratch(&h, &opened, "Rust");
        assert!(same(&inc, &full), "コメントを開き直したときにズレた");
    }

    #[test]
    fn 言語やテーマが変わったら行キャッシュを流用しない() {
        let h = Highlighter::new();
        let src = padded("// 先頭\n", 600);
        let _ = from_scratch(&h, &src, "Rust");
        // 同じ本文を別の言語で塗る (流用したら Rust の色のままになる)
        let as_rust = incremental(&h, &src, "Rust");
        let as_ts = h.layout_job(
            &src,
            "TypeScript",
            theme(),
            FontId::monospace(12.0),
            Color32::WHITE,
        );
        let fresh_ts = from_scratch(&h, &src, "TypeScript");
        assert!(
            same(&as_ts, &fresh_ts),
            "言語を変えたのに前の結果を流用した"
        );
        assert_eq!(as_rust.text, as_ts.text);
    }
}

// ===========================================================================
// 何万行のファイルでの正しさと費用
//
// syntect は行を跨いで状態を持つので、**見えている行だけを単独で塗ると
// 色が壊れる**。ここでは 5 万行の本文で「1 行目で開いた複数行コメントを
// 3 万行目で閉じる」極端な形を作り、3 万行目より前は全部コメント色、
// 4 万行目は本文色になることを固定する。
// ===========================================================================
#[cfg(test)]
mod huge_files {
    use super::*;

    fn theme_of(h: &Highlighter) -> &Theme {
        h.ts()
            .themes
            .get("base16-ocean.dark")
            .expect("既定テーマがある")
    }

    /// `lang` の構文で `text` を塗り、行ごとの塗り分けを返す。
    fn spans_of(h: &Highlighter, text: &str, lang: &str) -> Vec<Vec<LineSpan>> {
        let syntax = h.ps().find_syntax_by_name(lang).expect("構文が引ける");
        DOC_CACHE.with(|c| *c.borrow_mut() = None);
        highlight_doc(h.ps(), syntax, theme_of(h), text, Color32::WHITE, 0, None).spans
    }

    /// 5 万行。1 行目でブロックコメントを開き、3 万行目で閉じる。
    fn commented_50k() -> String {
        let mut s = String::from("/* ここから長いコメント\n");
        for i in 1..30_000 {
            s.push_str(&format!("コメントの中 {i}\n"));
        }
        s.push_str("*/\n");
        for i in 30_001..50_000 {
            s.push_str(&format!("let v{i} = {i};\n"));
        }
        s
    }

    #[test]
    fn 五万行の三万行目で閉じるコメントが正しく塗り分けられる() {
        let h = Highlighter::new();
        let src = commented_50k();
        let spans = spans_of(&h, &src, "Rust");
        assert!(spans.len() >= 49_999, "行数 {}", spans.len());
        let color = |line: usize| spans[line].first().map(|s| s.color);
        let cmt = color(100).expect("コメント行に色がある");
        // コメントの中はどこを取っても同じ色 (状態が引き継がれている)
        for l in [1usize, 5_000, 20_000, 29_998] {
            assert_eq!(color(l), Some(cmt), "{l} 行目がコメント色でない");
        }
        // 閉じたあとはコメント色ではない = 本文として塗られている
        for l in [30_001usize, 40_000, 45_000, 49_998] {
            assert_ne!(color(l), Some(cmt), "{l} 行目がコメントのままになっている");
        }
        // 4 万行目が「キーワード / 識別子 / 数値」に分かれていること
        let colors: std::collections::BTreeSet<[u8; 4]> =
            spans[40_000].iter().map(|s| s.color.to_array()).collect();
        assert!(colors.len() >= 3, "4 万行目の色数 {}", colors.len());
    }

    #[test]
    fn 極端に長い一行でも固まらず素通しになる() {
        // **絶対時間で線を引かない。** 以前ここは `took < 3 秒` を見ていたが、
        // 全件を同時実行したときだけ落ちる偽陽性を出していた
        // (このリポジトリで実測 3 件の前例がある罠)。守りたい性質は
        // 「行長に比例した費用が掛からない」ことなので、**行の長さを 2 倍にして
        // 出来上がる区間が 1 つも増えない**ことを見る。素通しであれば
        // 長さに依らず 1 区間で、塗っていれば長さに比例して区間が増える。
        let h = Highlighter::new();
        let long_line = |half: usize| format!("var a={};\n", "1+".repeat(half));
        let build = |half: usize| {
            let long = long_line(half);
            (format!("// 先頭\n{long}var b=2;\n"), long.len())
        };
        let (src_n, len_n) = build(1_250_000);
        let (src_2n, len_2n) = build(2_500_000);
        assert!(len_2n > 5_000_000, "2 倍側が上限を超えていない");

        let spans_n = spans_of(&h, &src_n, "JavaScript (Babel)");
        let spans_2n = spans_of(&h, &src_2n, "JavaScript (Babel)");

        for (spans, len, label) in [(&spans_n, len_n, "N"), (&spans_2n, len_2n, "2N")] {
            // 長すぎる行は 1 区間 (= 素通し) になる
            assert_eq!(spans[1].len(), 1, "{label}: 長すぎる行を塗ろうとしている");
            assert_eq!(spans[1][0].end as usize, len);
            // 前後の行は普通に塗れている (状態を壊していない)
            assert!(!spans[0].is_empty(), "{label}: 先頭行が塗れていない");
            assert!(spans[2].len() >= 2, "{label}: 長い行の後ろが塗れていない");
        }
        // 行を 2 倍にしても区間の総数が増えない = 行長に比例した費用が無い
        let total = |v: &[Vec<LineSpan>]| v.iter().map(|l| l.len()).sum::<usize>();
        assert_eq!(
            total(&spans_n),
            total(&spans_2n),
            "行長を 2 倍にしたら区間が増えた = 素通しになっていない"
        );
    }

    #[test]
    fn 上限を超える文書も本文が欠けずに色が付く() {
        let h = Highlighter::new();
        let src = "let x = 1;\n".repeat(MAX_HIGHLIGHT_BYTES / 11 + 100);
        assert!(src.len() > MAX_HIGHLIGHT_BYTES);
        let job = h.layout_job(
            &src,
            "Rust",
            "base16-ocean.dark",
            FontId::monospace(12.0),
            Color32::WHITE,
        );
        assert_eq!(job.text, src, "本文が欠けた");
        assert!(job.sections.len() > 1, "上限超えで塗るのを諦めている");
        let colored = job
            .sections
            .iter()
            .filter(|s| s.format.color != Color32::WHITE)
            .count();
        assert!(colored > 0, "どのセクションにも色が付いていない");
    }
}

// ===========================================================================
// 主要言語のハイライト品質
//
// 「拡張子から構文が引ける」だけでは、色が付いているかは分からない。
// ここでは 1 つの表を回して、言語ごとに**実際に塗り**、
//   * コメント / 文字列 / キーワード / 数値が**互いに違う色**になること
//   * どの色も背景に対して読めること (コントラスト比)
// までを見る。言語を足すときは表に 1 行足すだけで済むようにしてある。
// ===========================================================================
#[cfg(test)]
mod language_coverage {
    use super::*;
    use std::collections::BTreeSet;

    /// (ファイル名, 期待する言語名, 本文, コメント断片, 文字列断片,
    ///  キーワード断片, 数値断片)。断片が空文字なら、その検査は飛ばす。
    ///
    /// 断片は本文中で**最初に現れる位置**が目的のトークンになるように選ぶ
    /// (`zzcmt` / `zzstr` はそのための目印)。
    type Case = (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    );

    const CASES: &[Case] = &[
        // ---- 現代的なアプリ開発の主戦場 ----
        ("a.rs", "Rust",
         "// zzcmt\npub fn f() { let s = \"zzstr\"; let n = 1234567; }\n",
         "zzcmt", "zzstr", "pub", "1234567"),
        ("a.ts", "TypeScript",
         "// zzcmt\nexport const s: string = \"zzstr\";\nconst n = 1234567;\n",
         "zzcmt", "zzstr", "export", "1234567"),
        ("a.tsx", "TypeScriptReact",
         "// zzcmt\nexport const E = () => <div title=\"zzstr\">{1234567}</div>;\n",
         "zzcmt", "zzstr", "export", "1234567"),
        ("a.js", "JavaScript (Babel)",
         "// zzcmt\nexport const s = \"zzstr\";\nconst n = 1234567;\n",
         "zzcmt", "zzstr", "export", "1234567"),
        ("a.jsx", "JavaScript (Babel)",
         "// zzcmt\nexport const E = () => <div title=\"zzstr\">{1234567}</div>;\n",
         "zzcmt", "zzstr", "export", "1234567"),
        ("a.py", "Python",
         "# zzcmt\ndef f():\n    s = \"zzstr\"\n    n = 1234567\n",
         "zzcmt", "zzstr", "def", "1234567"),
        ("a.go", "Go",
         "// zzcmt\nfunc f() {\n\ts := \"zzstr\"\n\tn := 1234567\n\t_, _ = s, n\n}\n",
         "zzcmt", "zzstr", "func", "1234567"),
        ("a.java", "Java",
         "// zzcmt\npublic class C { String s = \"zzstr\"; int n = 1234567; }\n",
         "zzcmt", "zzstr", "public", "1234567"),
        ("a.kt", "Kotlin",
         "// zzcmt\nfun f() { val s = \"zzstr\"; val n = 1234567 }\n",
         "zzcmt", "zzstr", "fun", "1234567"),
        ("a.swift", "Swift",
         "// zzcmt\nfunc f() { let s = \"zzstr\"; let n = 1234567 }\n",
         "zzcmt", "zzstr", "func", "1234567"),
        ("a.c", "C",
         "/* zzcmt */\nint main(void) { const char *s = \"zzstr\"; return 1234567; }\n",
         "zzcmt", "zzstr", "return", "1234567"),
        ("a.cpp", "C++",
         "// zzcmt\ntemplate <class T> int f() { auto s = \"zzstr\"; return 1234567; }\n",
         "zzcmt", "zzstr", "template", "1234567"),
        ("a.cs", "C#",
         "// zzcmt\npublic class C { string s = \"zzstr\"; int n = 1234567; }\n",
         "zzcmt", "zzstr", "public", "1234567"),
        ("a.php", "PHP",
         "<?php\n// zzcmt\nfunction f() { $s = \"zzstr\"; $n = 1234567; }\n",
         "zzcmt", "zzstr", "function", "1234567"),
        ("a.rb", "Ruby",
         "# zzcmt\ndef f\n  s = \"zzstr\"\n  n = 1234567\nend\n",
         "zzcmt", "zzstr", "def", "1234567"),
        ("a.dart", "Dart",
         "// zzcmt\nvoid main() { var s = \"zzstr\"; var n = 1234567; }\n",
         "zzcmt", "zzstr", "void", "1234567"),
        ("a.zig", "Zig",
         "// zzcmt\npub fn main() void { const s = \"zzstr\"; const n = 1234567; _ = s; _ = n; }\n",
         "zzcmt", "zzstr", "pub", "1234567"),
        ("a.ex", "Elixir",
         "# zzcmt\ndefmodule M do\n  def f do\n    s = \"zzstr\"\n    n = 1234567\n    {s, n}\n  end\nend\n",
         "zzcmt", "zzstr", "defmodule", "1234567"),
        ("a.scala", "Scala",
         "// zzcmt\nobject O { val s = \"zzstr\"; val n = 1234567 }\n",
         "zzcmt", "zzstr", "object", "1234567"),
        ("a.hs", "Haskell",
         "-- zzcmt\nmain = do\n  let s = \"zzstr\"\n  let n = 1234567\n  print (s, n)\n",
         "zzcmt", "zzstr", "let", "1234567"),
        ("a.ml", "OCaml",
         "(* zzcmt *)\nlet s = \"zzstr\"\nlet n = 1234567\n",
         "zzcmt", "zzstr", "let", "1234567"),
        ("a.clj", "Clojure",
         "; zzcmt\n(def s \"zzstr\")\n(def n 1234567)\n",
         "zzcmt", "zzstr", "def", "1234567"),
        ("a.erl", "Erlang",
         "% zzcmt\n-module(m).\nf() -> S = \"zzstr\", N = 1234567, {S, N}.\n",
         "zzcmt", "zzstr", "module", "1234567"),
        ("a.r", "R",
         "# zzcmt\nf <- function() { s <- \"zzstr\"; n <- 1234567 }\n",
         "zzcmt", "zzstr", "function", "1234567"),
        ("a.jl", "Julia",
         "# zzcmt\nfunction f()\n    s = \"zzstr\"\n    n = 1234567\nend\n",
         "zzcmt", "zzstr", "function", "1234567"),
        ("a.pl", "Perl",
         "# zzcmt\nsub f { my $s = \"zzstr\"; my $n = 1234567; }\n",
         "zzcmt", "zzstr", "sub", "1234567"),
        ("a.lua", "Lua",
         "-- zzcmt\nlocal function f() local s = \"zzstr\" local n = 1234567 end\n",
         "zzcmt", "zzstr", "local", "1234567"),
        ("a.m", "Objective-C",
         "// zzcmt\n@implementation C\n- (int)f { NSString *s = @\"zzstr\"; return 1234567; }\n@end\n",
         "zzcmt", "zzstr", "return", "1234567"),
        ("a.mm", "Objective-C++",
         "// zzcmt\n@implementation C\n- (int)f { auto s = \"zzstr\"; return 1234567; }\n@end\n",
         "zzcmt", "zzstr", "return", "1234567"),
        ("a.groovy", "Groovy",
         "// zzcmt\ndef f() { def s = \"zzstr\"; def n = 1234567 }\n",
         "zzcmt", "zzstr", "def", "1234567"),
        ("a.cr", "Crystal",
         "# zzcmt\ndef f\n  s = \"zzstr\"\n  n = 1234567\nend\n",
         "zzcmt", "zzstr", "def", "1234567"),
        ("a.nim", "Nim",
         "# zzcmt\nproc f() =\n  let s = \"zzstr\"\n  let n = 1234567\n",
         "zzcmt", "zzstr", "proc", "1234567"),
        ("a.odin", "Odin",
         "// zzcmt\nf :: proc() { s := \"zzstr\"; n := 1234567 }\n",
         "zzcmt", "zzstr", "proc", "1234567"),
        ("a.elm", "Elm",
         "-- zzcmt\nmodule M exposing (..)\ns = \"zzstr\"\nn = 1234567\n",
         "zzcmt", "zzstr", "module", "1234567"),
        ("a.purs", "PureScript",
         "-- zzcmt\nmodule M where\ns = \"zzstr\"\nn = 1234567\n",
         "zzcmt", "zzstr", "module", "1234567"),
        ("a.rkt", "Racket",
         "; zzcmt\n(define s \"zzstr\")\n(define n 1234567)\n",
         "zzcmt", "zzstr", "define", "1234567"),
        ("a.scm", "Lisp",
         "; zzcmt\n(define s \"zzstr\")\n(define n 1234567)\n",
         "zzcmt", "zzstr", "define", "1234567"),
        ("a.tcl", "Tcl",
         "# zzcmt\nproc f {} { set s \"zzstr\"; set n 1234567 }\n",
         "zzcmt", "zzstr", "proc", "1234567"),
        ("a.d", "D",
         "// zzcmt\nvoid f() { auto s = \"zzstr\"; auto n = 1234567; }\n",
         "zzcmt", "zzstr", "void", "1234567"),
        ("a.sol", "Solidity",
         "// zzcmt\ncontract C { string s = \"zzstr\"; uint n = 1234567; }\n",
         "zzcmt", "zzstr", "contract", "1234567"),
        // ---- 科学計算・ハードウェア・低レイヤ ----
        ("a.f90", "Fortran (Modern)",
         "! zzcmt\nprogram p\n  character(*), parameter :: s = \"zzstr\"\n  integer :: n = 1234567\nend program p\n",
         "zzcmt", "zzstr", "program", "1234567"),
        ("a.adb", "Ada",
         "-- zzcmt\nprocedure P is\n   S : constant String := \"zzstr\";\n   N : Integer := 1234567;\nbegin\n   null;\nend P;\n",
         "zzcmt", "zzstr", "procedure", "1234567"),
        ("a.pas", "Pascal",
         "{ zzcmt }\nprogram P;\nconst S = 'zzstr';\nvar N: Integer = 1234567;\nbegin\nend.\n",
         "zzcmt", "zzstr", "program", "1234567"),
        ("a.asm", "x86_64 Assembly",
         "; zzcmt\nsection .data\nmsg db \"zzstr\", 0\nnum equ 1234567\n",
         "zzcmt", "zzstr", "section", "1234567"),
        ("a.v", "Verilog",
         "// zzcmt\nmodule m; reg [31:0] n = 1234567; initial $display(\"zzstr\"); endmodule\n",
         "zzcmt", "zzstr", "module", "1234567"),
        ("a.vhd", "VHDL",
         "-- zzcmt\nentity e is end entity;\narchitecture a of e is constant N : integer := 1234567; begin end;\n",
         "zzcmt", "", "entity", "1234567"),
        ("a.glsl", "GLSL",
         "// zzcmt\nvoid main() { float n = 1234567.0; gl_FragColor = vec4(n); }\n",
         "zzcmt", "", "void", "1234567"),
        ("a.wgsl", "WGSL",
         "// zzcmt\nfn main() { let n = 1234567; }\n",
         "zzcmt", "", "fn", "1234567"),
        ("a.matlab", "MATLAB",
         "% zzcmt\nfunction f()\n  s = 'zzstr';\n  n = 1234567;\nend\n",
         "zzcmt", "zzstr", "function", "1234567"),
        // ---- 記述・設定・データ ----
        ("a.html", "HTML",
         "<!-- zzcmt -->\n<div class=\"zzstr\">text</div>\n",
         "zzcmt", "zzstr", "div", ""),
        ("a.css", "CSS",
         "/* zzcmt */\n.a { color: red; content: \"zzstr\"; width: 1234567px; }\n",
         "zzcmt", "zzstr", "color", "1234567"),
        ("a.scss", "SCSS",
         "// zzcmt\n@mixin m { content: \"zzstr\"; width: 1234567px; }\n",
         "zzcmt", "zzstr", "@mixin", "1234567"),
        ("a.vue", "Vue Component",
         "<!-- zzcmt -->\n<template><div class=\"zzstr\">{{ msg }}</div></template>\n<script>export default { data() { return { n: 1234567 } } }</script>\n",
         "zzcmt", "zzstr", "export", "1234567"),
        ("a.svelte", "Svelte",
         "<!-- zzcmt -->\n<script>export let n = 1234567;</script>\n<div class=\"zzstr\">{n}</div>\n",
         "zzcmt", "zzstr", "export", "1234567"),
        ("a.xml", "XML",
         "<!-- zzcmt -->\n<root attr=\"zzstr\"><n>1</n></root>\n",
         "zzcmt", "zzstr", "root", ""),
        ("a.json", "JSON",
         "{\"kkk\": \"zzstr\", \"n\": 1234567, \"b\": true}\n",
         // JSON にキーワードは無く、キーも文字列スコープなので見ない
         "", "zzstr", "", "1234567"),
        ("a.yaml", "YAML",
         "# zzcmt\nkkk: \"zzstr\"\nn: 1234567\n",
         "zzcmt", "zzstr", "kkk", "1234567"),
        ("a.toml", "TOML",
         "# zzcmt\n[sect]\nkkk = \"zzstr\"\nn = 1234567\n",
         "zzcmt", "zzstr", "kkk", "1234567"),
        ("a.ini", "INI",
         "; zzcmt\n[sect]\nkkk = zzstr\n",
         "zzcmt", "", "kkk", ""),
        ("a.properties", "Java Properties",
         "# zzcmt\nkkk=zzstr\n",
         "zzcmt", "", "kkk", ""),
        ("a.md", "Markdown",
         "# zzcmt\n\n**zzstr** and `code` and [x](http://e)\n",
         "", "", "", ""),
        ("a.tex", "LaTeX",
         "% zzcmt\n\\documentclass{article}\n\\begin{document}zzstr\\end{document}\n",
         "zzcmt", "", "documentclass", ""),
        ("a.bib", "BibTeX",
         "% zzcmt\n@article{k, title = {zzstr}, year = 1234567}\n",
         "zzcmt", "", "article", ""),
        ("a.sql", "SQL",
         "-- zzcmt\nSELECT 'zzstr', 1234567 FROM t;\n",
         "zzcmt", "zzstr", "SELECT", "1234567"),
        ("a.graphql", "GraphQL",
         "# zzcmt\ntype Q { f(a: String = \"zzstr\", n: Int = 1234567): Int }\n",
         "zzcmt", "zzstr", "type", "1234567"),
        ("a.proto", "Protocol Buffer",
         "// zzcmt\nsyntax = \"zzstr\";\nmessage M { int32 n = 1234567; }\n",
         "zzcmt", "zzstr", "message", "1234567"),
        ("a.jsonnet", "jsonnet",
         "// zzcmt\n{ kkk: \"zzstr\", n: 1234567 }\n",
         "zzcmt", "zzstr", "", "1234567"),
        ("a.tf", "Terraform",
         "# zzcmt\nresource \"aws_s3_bucket\" \"b\" {\n  bucket = \"zzstr\"\n  count  = 1234567\n}\n",
         "zzcmt", "zzstr", "resource", "1234567"),
        ("a.nix", "Nix",
         "# zzcmt\nlet s = \"zzstr\"; n = 1234567; in s\n",
         "zzcmt", "zzstr", "let", "1234567"),
        ("a.rego", "Rego",
         "# zzcmt\npackage p\nallow { input.n == 1234567 }\n",
         "zzcmt", "", "package", "1234567"),
        ("a.typ", "Typst",
         "// zzcmt\n#let s = \"zzstr\"\n#let n = 1234567\n",
         "zzcmt", "zzstr", "let", "1234567"),
        ("a.jq", "JQ",
         "# zzcmt\n.a | select(.n == 1234567) | \"zzstr\"\n",
         "zzcmt", "zzstr", "select", "1234567"),
        // ---- シェル・ビルド・運用 ----
        ("a.sh", "Bourne Again Shell (bash)",
         "#!/bin/sh\n# zzcmt\nif true; then echo \"zzstr\" 1234567; fi\n",
         "zzcmt", "zzstr", "if", "1234567"),
        ("a.fish", "Fish",
         "# zzcmt\nfunction f\n  echo \"zzstr\" 1234567\nend\n",
         "zzcmt", "zzstr", "function", ""),
        ("a.ps1", "PowerShell",
         "# zzcmt\nfunction f { $s = \"zzstr\"; $n = 1234567 }\n",
         "zzcmt", "zzstr", "function", "1234567"),
        ("a.bat", "Batch File",
         "REM zzcmt\n@echo off\nset N=1234567\n",
         "zzcmt", "", "echo", ""),
        ("a.awk", "AWK",
         "# zzcmt\nfunction f() { s = \"zzstr\"; n = 1234567 }\n",
         "zzcmt", "zzstr", "function", "1234567"),
        ("a.vim", "VimL",
         "\" zzcmt\nfunction! F()\n  let s = \"zzstr\"\n  let n = 1234567\nendfunction\n",
         "zzcmt", "zzstr", "function", "1234567"),
        ("Makefile", "Makefile",
         "# zzcmt\nVAR = zzstr\nall:\n\t@echo $(VAR)\n",
         "zzcmt", "", "all", ""),
        ("CMakeLists.txt", "CMake",
         "# zzcmt\nproject(P)\nset(S \"zzstr\")\nset(N 1234567)\n",
         "zzcmt", "zzstr", "project", ""),
        ("Dockerfile", "Dockerfile",
         "# zzcmt\nFROM alpine:3\nRUN echo \"zzstr\"\n",
         "zzcmt", "zzstr", "FROM", ""),
        ("a.diff", "Diff",
         "--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-old line\n+new line\n",
         "", "", "", ""),
        ("COMMIT_EDITMSG", "Git Commit",
         "件名の行\n\n本文\n# zzcmt\n",
         "zzcmt", "", "", ""),
        ("a.gitignore", "Git Ignore",
         "# zzcmt\n/target\n*.log\n",
         "zzcmt", "", "", ""),
        // ---- 拡張セットに無く、同梱パックが埋めている言語 ----
        ("a.hx", "Haxe",
         "// zzcmt\nclass C { function f() { var s = \"zzstr\"; var n = 1234567; } }\n",
         "zzcmt", "zzstr", "class", "1234567"),
        ("a.vala", "Vala",
         "// zzcmt\nclass C { void f() { string s = \"zzstr\"; int n = 1234567; } }\n",
         "zzcmt", "zzstr", "class", "1234567"),
        ("a.gleam", "Gleam",
         "// zzcmt\npub fn f() { let s = \"zzstr\" let n = 1234567 }\n",
         "zzcmt", "zzstr", "pub", "1234567"),
        ("a.res", "ReScript",
         "// zzcmt\nlet s = \"zzstr\"\nlet n = 1234567\n",
         "zzcmt", "zzstr", "let", "1234567"),
        ("a.bicep", "Bicep",
         "// zzcmt\nparam s string = 'zzstr'\nvar n = 1234567\n",
         "zzcmt", "zzstr", "param", "1234567"),
        ("a.thrift", "Thrift",
         "// zzcmt\nstruct S { 1: string s = \"zzstr\"; 2: i32 n = 1234567; }\n",
         "zzcmt", "zzstr", "struct", "1234567"),
        ("a.vb", "Visual Basic",
         "' zzcmt\nModule M\n  Dim S As String = \"zzstr\"\n  Dim N As Integer = 1234567\nEnd Module\n",
         "zzcmt", "zzstr", "Module", "1234567"),
        ("a.wat", "WebAssembly",
         ";; zzcmt\n(module (func $f (result i32) (i32.const 1234567)))\n",
         "zzcmt", "", "module", "1234567"),
        ("a.pro", "Prolog",
         "% zzcmt\nf(X) :- X = \"zzstr\", Y = 1234567, write(Y).\n",
         "zzcmt", "zzstr", "write", "1234567"),
        ("justfile", "Just",
         "# zzcmt\nbuild:\n    cargo build\n",
         "zzcmt", "", "", ""),
        ("a.nginx", "Nginx",
         "# zzcmt\nserver {\n  listen 1234567;\n  root \"zzstr\";\n}\n",
         "zzcmt", "zzstr", "server", "1234567"),
        // ---- 拡張セットにも既存パックにも無く、今回埋めた 7 言語 ----
        ("a.cue", "CUE",
         "// zzcmt\npackage p\ns: \"zzstr\"\nn: 1234567\n",
         "zzcmt", "zzstr", "package", "1234567"),
        // KDL に予約語は無い。仕様が「キーワード値」と呼ぶ `#true` を見る
        ("a.kdl", "KDL",
         "// zzcmt\nnode \"zzstr\" flag=#true count=1234567\n",
         "zzcmt", "zzstr", "#true", "1234567"),
        ("a.dhall", "Dhall",
         "-- zzcmt\nlet s = \"zzstr\"\nlet n = 1234567\nin { s = s, n = n }\n",
         "zzcmt", "zzstr", "let", "1234567"),
        ("a.cob", "COBOL",
         "      *> zzcmt\n       PROCEDURE DIVISION.\n           DISPLAY \"zzstr\".\n           MOVE 1234567 TO WS-N.\n",
         "zzcmt", "zzstr", "PROCEDURE", "1234567"),
        ("a.apex", "Apex",
         "// zzcmt\npublic class C { String s = 'zzstr'; Integer n = 1234567; }\n",
         "zzcmt", "zzstr", "public", "1234567"),
        // sed に文字列リテラルは無いので、文字列の検査だけ飛ばす
        ("a.sed", "sed",
         "# zzcmt\ns/foo/bar/g\nq 1234567\n",
         "zzcmt", "", "s/", "1234567"),
        ("a.vv", "V",
         "// zzcmt\nfn main() {\n\ts := 'zzstr'\n\tn := 1234567\n\tprintln('${s} ${n}')\n}\n",
         "zzcmt", "zzstr", "fn", "1234567"),
    ];

    fn hl() -> Highlighter {
        let h = Highlighter::new();
        h.set_grammars(crate::grammar::bundled_pack::load());
        h
    }

    /// 相対輝度 (WCAG 2.x)。
    fn luminance(c: Color32) -> f32 {
        let f = |v: u8| {
            let s = v as f32 / 255.0;
            if s <= 0.03928 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
    }

    /// コントラスト比 (1.0 = 同じ, 21.0 = 白と黒)。
    fn contrast(a: Color32, b: Color32) -> f32 {
        let (x, y) = (luminance(a), luminance(b));
        let (hi, lo) = if x > y { (x, y) } else { (y, x) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// 「読める」とみなす最低コントラスト比。
    ///
    /// コメントは意図的に沈めるので WCAG AA (4.5) は満たさない。
    /// ここで見たいのは「背景と同化していない」ことなので、
    /// **地の文が背景と区別できる下限**として 1.8 を採る
    /// (全テーマ・全言語の実測の最小が 2.1 だったので、少し余裕を持たせた値)。
    const MIN_CONTRAST: f32 = 1.8;

    /// `needle` が最初に現れる位置の色。
    fn color_of(job: &LayoutJob, needle: &str) -> Color32 {
        let at = job
            .text
            .find(needle)
            .unwrap_or_else(|| panic!("本文に {needle:?} が無い"));
        job.sections
            .iter()
            .find(|s| s.byte_range.contains(&at))
            .map(|s| s.format.color)
            .unwrap_or_else(|| panic!("{needle:?} を覆う区間が無い"))
    }

    #[test]
    fn 表に載せた全ての言語が拡張子から解決される() {
        let h = hl();
        let mut wrong = Vec::new();
        for (file, want, src, ..) in CASES {
            let got = h.lang_for(Some(Path::new(file)), src);
            if &got != want {
                wrong.push(format!("{file}: {got} (期待 {want})"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
        // 要求水準 (主要 50 言語) を割ったら気づけるように
        assert!(CASES.len() >= 50, "対応言語が減っている: {}", CASES.len());
    }

    #[test]
    fn 全ての言語でコメントと文字列とキーワードと数値が別の色になる() {
        let h = hl();
        let theme = "base16-ocean.dark";
        let bg = h
            .ts()
            .themes
            .get(theme)
            .and_then(|t| t.settings.background)
            .map(|c| Color32::from_rgb(c.r, c.g, c.b))
            .expect("既定テーマに背景色がある");
        let mut bad: Vec<String> = Vec::new();
        for (file, lang, src, cmt, string, kw, num) in CASES {
            let job = h.layout_job(src, lang, theme, FontId::monospace(12.0), Color32::WHITE);
            if job.text != *src {
                bad.push(format!("{file}: 本文が欠けた"));
                continue;
            }
            // どの言語も最低 3 色には分かれること (単色で塗られていない)
            let distinct: BTreeSet<[u8; 4]> = job
                .sections
                .iter()
                .map(|s| s.format.color.to_array())
                .collect();
            let named: Vec<(&str, &str)> = [
                ("コメント", *cmt),
                ("文字列", *string),
                ("キーワード", *kw),
                ("数値", *num),
            ]
            .into_iter()
            .filter(|(_, n)| !n.is_empty())
            .collect();
            // 色の下限。コメントしか無い言語 (Git Commit / justfile) に
            // 3 色を求めても意味が無いので、見る断片の数から決める。
            let want = if named.len() >= 2 { 3 } else { 2 };
            if distinct.len() < want {
                bad.push(format!("{file} ({lang}): 色数 {}", distinct.len()));
            }
            // 指定した断片は互いに違う色で、かつ背景から浮いていること
            let mut seen: Vec<(&str, Color32)> = Vec::new();
            for (what, needle) in named {
                let c = color_of(&job, needle);
                if contrast(c, bg) < MIN_CONTRAST {
                    bad.push(format!(
                        "{file} ({lang}): {what} が背景に埋もれている (比 {:.2})",
                        contrast(c, bg)
                    ));
                }
                if let Some((prev, _)) = seen.iter().find(|(_, pc)| *pc == c) {
                    bad.push(format!("{file} ({lang}): {what} と {prev} が同じ色"));
                }
                seen.push((what, c));
            }
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }
}

// ===========================================================================
// Rust だけは細部まで見る
//
// 自分自身を書く言語なので、ライフタイム注記・マクロ・属性・raw string・
// 生ポインタ・async/await・入れ子のジェネリクス・doc コメント・unsafe・
// where 節・シェバンまで、代表的な書き方が**互いに違う色**になることを固定する。
// ===========================================================================
#[cfg(test)]
mod rust_quality {
    use super::*;

    const SRC: &str = r##"#!/usr/bin/env rust-script
//! クレートの doc コメント
use std::collections::HashMap;

/// 関数の doc コメント
#[derive(Debug, Clone, PartialEq)]
pub struct Holder<'a, T> {
    borrowed: &'a str,
    items: Vec<HashMap<String, Vec<u8>>>,
    marker: std::marker::PhantomData<T>,
}

macro_rules! twice {
    ($x:expr) => { $x + $x };
}

impl<'a, T> Holder<'a, T>
where
    T: Send + 'static,
{
    pub async fn fetch(&self, count: usize) -> Result<u64, String> {
        let raw = r#"生の "文字列" はエスケープしない"#;
        let normal = "普通の文字列\n";
        let hex = 0xDEAD_BEEF_u64;
        let float = 1.5e-3;
        unsafe {
            let ptr: *const u8 = std::ptr::null();
            if !ptr.is_null() {
                return Err(normal.to_string());
            }
        }
        let doubled = twice!(count);
        let joined = other().await;
        Ok(hex + doubled as u64 + joined + float as u64 + raw.len() as u64)
    }
}

async fn other() -> u64 { 1 }

pub trait Render {}

impl<T: Display + Clone> Render for T {}

fn label<T>(v: T) -> String
where
    T: Into<String>,
{
    v.into()
}
"##;

    fn hl() -> Highlighter {
        Highlighter::new()
    }

    /// 同梱 Rust 構文セットに入ってよい構文の数の上限。
    ///
    /// 拡張セット (数百種) へ混ぜると初回の組み立てが桁で遅くなる。
    /// **速さではなく大きさで縛る**のは、機械の速さに依存しない検査に
    /// するため (絶対時間の閾値は負荷で必ず嘘をつく)。
    const RUST_SYNTAX_MAX: usize = 16;

    fn colors(h: &Highlighter, theme: &str) -> LayoutJob {
        h.layout_job(SRC, "Rust", theme, FontId::monospace(12.0), Color32::WHITE)
    }

    /// `needle` が最初に現れる位置の色。
    fn at(job: &LayoutJob, needle: &str) -> Color32 {
        let i = job
            .text
            .find(needle)
            .unwrap_or_else(|| panic!("本文に {needle:?} が無い"));
        job.sections
            .iter()
            .find(|s| s.byte_range.contains(&i))
            .map(|s| s.format.color)
            .unwrap_or_else(|| panic!("{needle:?} を覆う区間が無い"))
    }

    #[test]
    fn 代表的なrustの書き方が互いに違う色になる() {
        let h = hl();
        let job = colors(&h, "base16-ocean.dark");
        assert_eq!(job.text, SRC, "本文が欠けた");
        // 比較の基準は**テーマの地の色 (本文の色)**。
        //
        // 同梱した新しい Rust 構文は `let` の束縛名や式中の識別子にも
        // `variable.other` を付ける (Sublime Text 本体と同じ塗り方) ので、
        // 「どのスコープも付いていない素の識別子」という基準そのものが無くなった。
        // ここで見たいのは「地の文と見分けが付くか」なので前景色を基準にする。
        let body = h
            .ts()
            .themes
            .get("base16-ocean.dark")
            .and_then(|t| t.settings.foreground)
            .map(|c| Color32::from_rgb(c.r, c.g, c.b))
            .expect("既定テーマに前景色がある");
        // (見たいもの, 本文中の断片) — 基準色と違うこと
        let differs: &[(&str, &str)] = &[
            ("ライフタイム注記", "'a,"),
            ("async fn の fn", "fn fetch"),
            ("マクロ定義", "macro_rules"),
            ("属性", "derive"),
            ("doc コメント (///)", "関数の doc"),
            ("crate doc コメント (//!)", "クレートの doc"),
            ("raw string", "生の "),
            ("通常の文字列", "普通の文字列"),
            ("16 進リテラル", "0xDEAD_BEEF"),
            ("浮動小数リテラル", "1.5e-3"),
            ("unsafe", "unsafe"),
            ("where 節", "where"),
            ("生ポインタの型", "*const u8"),
            // ここから下は同梱の `assets/syntaxes/Rust.sublime-syntax` で
            // 初めて色が付くようになったもの (two-face が持つ古い版では
            // 全て素の識別子と同じ色だった)。回帰したら必ずここで落ちる。
            ("async", "async fn"),
            (".await の await", "await;"),
            ("トレイト境界の型", "Display + Clone"),
            // where 節の型パラメータ (`T`) は `storage.type` が付く。
            // 境界名そのもの (`Into`) は `support.type` で、base16 系の
            // tmTheme はこのスコープに色を定義していない (= 構文ではなく
            // テーマ側の都合。two-face の古い構文でも同じだった)。
            ("where 節の型パラメータ", "T: Into"),
            ("シェバン", "/usr/bin/env"),
        ];
        let mut bad = Vec::new();
        for (what, needle) in differs {
            if at(&job, needle) == body {
                bad.push(format!("{what} ({needle:?}) が地の文と同じ色"));
            }
        }
        // 文字列は raw でも通常でも同じ扱い、コメントとは別の色
        let s_raw = at(&job, "生の ");
        let s_norm = at(&job, "普通の文字列");
        let cmt = at(&job, "関数の doc");
        if s_raw != s_norm {
            bad.push("raw string と通常の文字列で色が違う".into());
        }
        if s_raw == cmt {
            bad.push("文字列とコメントが同じ色".into());
        }
        // 数値はコメントとも文字列とも違う
        if at(&job, "0xDEAD_BEEF") == s_raw || at(&job, "0xDEAD_BEEF") == cmt {
            bad.push("数値が文字列かコメントと同じ色".into());
        }
        // 属性名が「コメント / 文字列 / 数値 / キーワード」のどれかに
        // 紛れ込んでいないこと (地の文と違うだけでは分類として弱いため)。
        let attr = at(&job, "derive");
        for (what, c) in [
            ("コメント", cmt),
            ("文字列", s_raw),
            ("数値", at(&job, "0xDEAD_BEEF")),
            ("キーワード", at(&job, "unsafe")),
        ] {
            if attr == c {
                bad.push(format!("属性が{what}と同じ色"));
            }
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }

    #[test]
    fn シェバンで始まっても後続のrustが壊れない() {
        let h = hl();
        let job = colors(&h, "base16-ocean.dark");
        // シェバン行はコメントとして塗られ、**その後ろも壊れないこと**が要点。
        let body = h
            .ts()
            .themes
            .get("base16-ocean.dark")
            .and_then(|t| t.settings.foreground)
            .map(|c| Color32::from_rgb(c.r, c.g, c.b))
            .expect("既定テーマに前景色がある");
        assert_eq!(
            at(&job, "/usr/bin/env"),
            at(&job, "関数の doc"),
            "シェバンがコメント色で塗られていない"
        );
        assert_ne!(at(&job, "pub struct"), body, "シェバンの後ろが素通し");
        assert_ne!(at(&job, "普通の文字列"), body, "文字列が塗れていない");
        assert_ne!(at(&job, "関数の doc"), body, "doc コメントが塗れていない");
        assert_ne!(at(&job, "macro_rules"), body, "マクロが塗れていない");
    }

    #[test]
    fn 全てのカラーテーマでrustのコメントと本文が別の色になる() {
        let h = hl();
        let mut bad = Vec::new();
        for t in crate::theme::all() {
            let job = colors(&h, &t.syntect_theme);
            let cmt = at(&job, "関数の doc");
            let ident = at(&job, "doubled");
            let string = at(&job, "普通の文字列");
            let bg = h
                .ts()
                .themes
                .get(&t.syntect_theme)
                .and_then(|s| s.settings.background)
                .map(|c| Color32::from_rgb(c.r, c.g, c.b));
            if cmt == ident {
                bad.push(format!("{}: コメントと本文が同じ色", t.name));
            }
            if cmt == string {
                bad.push(format!("{}: コメントと文字列が同じ色", t.name));
            }
            if Some(cmt) == bg {
                bad.push(format!("{}: コメントが背景と同じ色", t.name));
            }
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// 同梱の Rust 構文は **Rust を実際に塗るまで**組まない。
    ///
    /// `SyntaxSet::into_builder()` 経由で拡張セットへ混ぜると 220 構文を
    /// 全部張り直すことになり、実測 (release) で 1 回 0.6〜0.8 秒かかる。
    /// 別セットに切り出したうえで遅延させている、という設計をここで固定する。
    #[test]
    fn rust構文は塗るまで組まれない() {
        let h = hl();
        assert!(
            h.rust_ps.get().is_none(),
            "作っただけで Rust 構文を組んでいる"
        );
        // 他言語の判定・着色では触らない
        assert_eq!(h.lang_for(Some(Path::new("a.py")), ""), "Python");
        let _ = h.layout_job(
            "x = 1\n",
            "Python",
            "base16-ocean.dark",
            FontId::monospace(12.0),
            Color32::WHITE,
        );
        assert!(
            h.rust_ps.get().is_none(),
            "他言語を塗っただけで Rust 構文を組んでいる"
        );

        let t = std::time::Instant::now();
        let job = colors(&h, "base16-ocean.dark");
        let first = t.elapsed();
        assert_eq!(job.text, SRC, "本文が欠けた");
        assert!(
            h.rust_ps.get().and_then(|o| o.as_ref()).is_some(),
            "Rust を塗ったのに専用セットが組まれていない"
        );
        // **絶対時間で線を引かない。** 守りたいのは「Rust 専用の小さな構文
        // セットを組む」ことであって速さそのものではない。固定の閾値は
        // 「手元では通り、全 4251 件と同時に走らせた負荷では落ちる」試験に
        // なる (実測 1.79 秒で落ちた)。速さはセットの大きさで決まるので、
        // **大きさを直に見る** — これなら機械の速さに 1 ミリも依存しない。
        let set = h
            .rust_ps
            .get()
            .and_then(|o| o.as_ref())
            .expect("組まれている");
        let n = set.syntaxes().len();
        eprintln!("同梱 Rust 構文: {n} 種 / 初回組み立て {first:?}");
        assert!(
            n <= RUST_SYNTAX_MAX,
            "拡張セットへ混ぜる実装へ戻っている: {n} 種 (上限 {RUST_SYNTAX_MAX})"
        );
        // 桁違いに遅いのだけは拾う (壊れ方の smoke check。負荷では余裕を持たせる)。
        assert!(
            first < std::time::Duration::from_secs(15),
            "同梱 Rust 構文の初回組み立てが桁違いに遅い: {first:?}"
        );
    }

    /// 同梱した第三者ファイルの出典とライセンスが、ファイル自身と
    /// 添付のライセンスファイルの**両方**に書いてあること。
    #[test]
    fn 同梱したrust構文に出典とライセンスが書いてある() {
        // Windows のチェックアウトは CRLF なので必ず正規化してから探す。
        let yaml = RUST_SYNTAX_YAML.replace("\r\n", "\n");
        for needle in [
            "https://github.com/sublimehq/Packages",
            "Permission to copy, use, modify, sell and distribute",
        ] {
            assert!(yaml.contains(needle), "構文定義に記載が無い: {needle}");
        }
        let lic = include_str!("../assets/syntaxes/Rust.sublime-syntax-license.txt")
            .replace("\r\n", "\n");
        for needle in [
            "https://github.com/sublimehq/Packages",
            "Permission to copy, use, modify, sell and distribute",
            "assets/syntaxes/Rust.sublime-syntax",
        ] {
            assert!(lic.contains(needle), "添付ライセンスに記載が無い: {needle}");
        }
    }

    #[test]
    fn カラーテーマのsyntect名が全て解決できる() {
        let h = hl();
        let mut missing = Vec::new();
        for t in crate::theme::all() {
            if !h.ts().themes.contains_key(&t.syntect_theme) {
                missing.push(format!("{} → {}", t.name, t.syntect_theme));
            }
        }
        assert!(missing.is_empty(), "解決できないテーマ名: {missing:#?}");
        // 拡張セットぶんテーマが増えていること (Dracula / Nord / gruvbox …)
        let defaults = ThemeSet::load_defaults().themes.len();
        assert!(
            h.ts().themes.len() > defaults,
            "テーマが増えていない: {} (既定 {defaults})",
            h.ts().themes.len()
        );
        // 端末専用テーマは混ぜない
        for name in TERMINAL_ONLY_THEMES {
            assert!(!h.ts().themes.contains_key(*name), "{name} を混ぜている");
        }
    }
}

// ===========================================================================
// 可視域だけを塗る経路 (巨大ファイル)
//
// ここで守りたいのは 3 つ。
//   * **どんな行数でも色が付く** — 先頭・中間・末尾のどこを見ても、
//     トークンが 1 種類ということが無い
//   * **チェックポイントから再開した結果が、先頭から通した結果と一致する**
//     — ここがずれると文字列やコメントの色が延々と尾を引く
//   * **1 回の塗りの費用が頭打ちになる** — 実時間では測らない。
//     「syntect へ通した行数」という守りたい性質そのものを数える
// ===========================================================================
#[cfg(test)]
mod visible_window {
    use super::*;
    use std::collections::BTreeSet;

    fn theme() -> &'static str {
        "base16-ocean.dark"
    }

    fn font() -> FontId {
        FontId::monospace(12.0)
    }

    /// 台帳を空にする (テスト同士が前の文書の足場を拾わないように)。
    fn reset() {
        WIN_CACHE.with(|c| *c.borrow_mut() = None);
        DOC_CACHE.with(|c| *c.borrow_mut() = None);
    }

    /// 各行の先頭バイト位置 (行数 + 1 個。末尾は本文長)。
    fn line_starts(text: &str) -> Vec<usize> {
        let mut v = vec![0usize];
        for l in LinesWithEndings::from(text) {
            v.push(v[v.len() - 1] + l.len());
        }
        v
    }

    /// `range` に掛かるセクションの色を集める (空白だけの区間は数えない)。
    fn palette_in(job: &LayoutJob, range: std::ops::Range<usize>) -> BTreeSet<(u8, u8, u8)> {
        let mut set = BTreeSet::new();
        for s in &job.sections {
            if s.byte_range.end <= range.start || s.byte_range.start >= range.end {
                continue;
            }
            let a = s.byte_range.start.max(range.start);
            let b = s.byte_range.end.min(range.end);
            if job.text[a..b].trim().is_empty() {
                continue;
            }
            let c = s.format.color;
            set.insert((c.r(), c.g(), c.b()));
        }
        set
    }

    /// セクションが本文を隙間なく・重なりなく覆っていること。
    fn assert_covered(job: &LayoutJob) {
        let mut at = 0usize;
        for s in &job.sections {
            assert_eq!(s.byte_range.start, at, "セクションに隙間か重なりがある");
            assert!(s.byte_range.end >= s.byte_range.start, "逆順のセクション");
            at = s.byte_range.end;
        }
        assert_eq!(at, job.text.len(), "本文の末尾まで覆えていない");
    }

    /// 追い付くまで pump する。返り値は回った回数。
    fn pump(h: &Highlighter, text: &str, lang: &str, first: usize, rows: usize) -> usize {
        let win = snap_window(first, rows);
        for i in 1..1_000_000usize {
            let a = h.advance_to_visible(text, lang, theme(), Color32::WHITE, win);
            if a.ready {
                return i;
            }
            assert!(
                a.scanned_lines <= WINDOW_SCAN_BUDGET_LINES,
                "1 回の追い付きが予算を超えた: {}",
                a.scanned_lines
            );
        }
        panic!("追い付かない");
    }

    fn paint(
        h: &Highlighter,
        text: &str,
        lang: &str,
        first: usize,
        rows: usize,
    ) -> super::VisibleJob {
        h.layout_job_visible(
            text,
            lang,
            theme(),
            font(),
            Color32::WHITE,
            snap_window(first, rows),
        )
    }

    /// このリポジトリで一番大きな実ソース。無ければ手元のファイルを
    /// 繰り返して嵩を出す (どの環境でも [`MAX_HIGHLIGHT_BYTES`] を超える)。
    fn 巨大な実ソース() -> String {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        if let Ok(s) = std::fs::read_to_string(root.join("src/app.rs")) {
            if s.len() > MAX_HIGHLIGHT_BYTES {
                return s;
            }
        }
        let one = std::fs::read_to_string(root.join("src/highlight.rs")).expect("自分自身は読める");
        let mut s = String::new();
        while s.len() <= MAX_HIGHLIGHT_BYTES * 3 {
            s.push_str(&one);
        }
        s
    }

    /// 行を跨ぐ構文 (`/* */` と生文字列) を含む合成ソース。
    /// `MAX_HIGHLIGHT_BYTES` を確実に超える大きさにする。
    fn 行を跨ぐ構文を含む合成ソース() -> String {
        let mut s = String::new();
        let mut i = 0usize;
        while s.len() <= MAX_HIGHLIGHT_BYTES + 64 * 1024 {
            // 1 かたまり 13 行。ブロックコメントと生文字列が行を跨ぐ。
            // **13 は 512 ([`WINDOW_BLOCK_LINES`]) と互いに素**にしてある —
            // かたまりの長さが割り切れると、ブロック境界がいつも同じ行種に
            // 当たってしまい「行を跨ぐ構文の途中で始まる可視域」を作れない。
            s.push_str(&format!("fn f{i}(a: u32) -> u32 {{\n"));
            s.push_str("    /* ここから\n");
            s.push_str("       さらにコメント\n");
            s.push_str("       行を跨ぐ\n");
            s.push_str("       ブロックコメント */\n");
            s.push_str(&format!("    let s = r#\"生の文字列 {i}\n"));
            s.push_str("       まだ文字列の中 fn let 123\n");
            s.push_str("       ここで閉じる\"#;\n");
            s.push_str(&format!("    let n = {i} + 0x2a; // 行コメント\n"));
            s.push_str("    let _ = s;\n");
            s.push_str("    n + a\n");
            s.push_str("}\n");
            s.push('\n');
            i += 1;
        }
        s
    }

    #[test]
    fn 可視域の丸めは外側へ広がる() {
        // (先頭行, 行数) -> (開始, 終了)
        let b = WINDOW_BLOCK_LINES;
        for (first, rows, want) in [
            (0usize, 1usize, (0usize, b)),
            (0, b, (0, b)),
            (1, b, (0, 2 * b)),
            (b, 10, (b, 2 * b)),
            (b - 1, 1, (0, b)),
            (3 * b + 7, 40, (3 * b, 4 * b)),
        ] {
            let w = snap_window(first, rows);
            assert_eq!((w.start, w.end), want, "first={first} rows={rows}");
            let (s, e) = (w.start, w.end);
            assert!(
                s <= first && first + rows.max(1) <= e,
                "可視域を覆えていない"
            );
            assert!(s.is_multiple_of(b) && e.is_multiple_of(b), "境界が粗くない");
        }
    }

    #[test]
    fn 巨大な実ソースの先頭と中間と末尾に色が付く() {
        let text = 巨大な実ソース();
        assert!(
            text.len() > MAX_HIGHLIGHT_BYTES,
            "入力が上限を超えていない: {}",
            text.len()
        );
        let starts = line_starts(&text);
        let n = starts.len() - 1;
        let h = Highlighter::new();
        let rows = 50usize;
        for (name, first) in [
            ("先頭", 0usize),
            ("中間", (n / 2).min(20_000)),
            ("末尾", n.saturating_sub(rows + 1)),
        ] {
            reset();
            let v = paint(&h, &text, "Rust", first, rows);
            assert_covered(&v.job);
            assert_eq!(v.job.text, text, "{name}: 本文が欠けた");
            let Window { start: w0, end: w1 } = snap_window(first, rows);
            let (w0, w1) = (w0.min(n), w1.min(n));
            let pal = palette_in(&v.job, starts[w0]..starts[w1]);
            assert!(
                pal.len() >= 3,
                "{name} (行 {first}) が塗られていない: 色 {} 種類",
                pal.len()
            );
        }
    }

    #[test]
    fn 追い付く前でも可視域は白一色にならない() {
        let text = 行を跨ぐ構文を含む合成ソース();
        let starts = line_starts(&text);
        let n = starts.len() - 1;
        let h = Highlighter::new();
        reset();
        // 台帳が空のまま遠くを塗る = 暫定表示。
        let first = n / 2;
        let v = paint(&h, &text, "Rust", first, 50);
        assert!(!v.exact, "この位置は 1 回では追い付けないはず");
        let Window { start: w0, end: w1 } = snap_window(first, 50);
        let pal = palette_in(&v.job, starts[w0.min(n)]..starts[w1.min(n)]);
        assert!(pal.len() >= 3, "暫定表示が単色になっている: {}", pal.len());
    }

    #[test]
    fn チェックポイントから再開した結果は先頭から通したものと一致する() {
        let text = 行を跨ぐ構文を含む合成ソース();
        let starts = line_starts(&text);
        let n = starts.len() - 1;
        let lines: Vec<&str> = LinesWithEndings::from(text.as_str()).collect();
        let h = Highlighter::new();

        // 先頭から通した答え (既存の全文経路)。
        let (set, syntax) = h.syntax_for("Rust").expect("Rust の構文がある");
        let theme_ref = h.ts().themes.get(theme()).expect("既定テーマがある");
        reset();
        let want = highlight_doc(set, syntax, theme_ref, &text, Color32::WHITE, 0, None);
        let want_job = job_from_spans(&text, &lines, &want.spans, &font());

        // 行を跨ぐ構文の途中で始まる可視域を選ぶ (ここがずれると尾を引く)。
        // 生文字列の 2 行目が来る所を狙って、ブロック境界を後ろから探す。
        let mut first = None;
        for b in (1..n / WINDOW_BLOCK_LINES).rev() {
            let l = lines[b * WINDOW_BLOCK_LINES];
            if l.trim_start().starts_with("まだ文字列の中")
                || l.trim_start().starts_with("行を跨ぐ")
            {
                first = Some(b * WINDOW_BLOCK_LINES);
                break;
            }
        }
        let first = first.expect("行を跨ぐ構文の途中に当たるブロック境界がある");
        let Window { start: w0, end: w1 } = snap_window(first, 1);
        let (w0, w1) = (w0.min(n), w1.min(n));
        let range = starts[w0]..starts[w1];

        // (1) 追い付く前 = 暫定表示は、正解と違っていてよい (むしろ違う)。
        reset();
        let prov = paint(&h, &text, "Rust", first, 1);
        assert!(!prov.exact, "この位置は暫定表示になるはず");
        // 色の**集合**は可視域 512 行ぶんを均せば一致してしまうので、
        // 1 バイトずつ突き合わせる。
        assert_ne!(
            colors_of(&prov.job, range.clone()),
            colors_of(&want_job, range.clone()),
            "暫定表示が正解と同じ = この検査は再開の失敗を捕まえられない"
        );

        // (2) 追い付いたら、先頭から通した結果と**バイト単位で**一致する。
        pump(&h, &text, "Rust", first, 1);
        let got = paint(&h, &text, "Rust", first, 1);
        assert!(got.exact, "追い付いたのに暫定表示のまま");
        assert_covered(&got.job);
        let a = colors_of(&got.job, range.clone());
        let b = colors_of(&want_job, range.clone());
        assert_eq!(a.len(), b.len());
        let diff = a.iter().zip(&b).filter(|(x, y)| x != y).count();
        assert_eq!(diff, 0, "再開した結果が {diff} バイトぶんずれている");
    }

    /// `range` の 1 バイトごとの色。
    fn colors_of(job: &LayoutJob, range: std::ops::Range<usize>) -> Vec<(u8, u8, u8)> {
        let mut out = vec![(0u8, 0u8, 0u8); range.len()];
        for s in &job.sections {
            if s.byte_range.end <= range.start || s.byte_range.start >= range.end {
                continue;
            }
            let a = s.byte_range.start.max(range.start);
            let b = s.byte_range.end.min(range.end);
            let c = s.format.color;
            for x in a..b {
                out[x - range.start] = (c.r(), c.g(), c.b());
            }
        }
        out
    }

    #[test]
    fn 可視域を塗るのに舐める行数はチェックポイント間隔で頭打ちになる() {
        let text = 行を跨ぐ構文を含む合成ソース();
        let n = line_starts(&text).len() - 1;
        let h = Highlighter::new();
        let first = (n / 2) - (n / 2) % WINDOW_BLOCK_LINES;
        reset();
        pump(&h, &text, "Rust", first, 50);
        let v = paint(&h, &text, "Rust", first, 50);
        assert!(v.exact);
        let Window { start: w0, end: w1 } = snap_window(first, 50);
        let window = w1.min(n) - w0.min(n);
        let ceiling = CHECKPOINT_LINES + window + WINDOW_SCAN_BUDGET_LINES;
        assert!(
            v.scanned_lines <= ceiling,
            "{first} 行目を塗るのに {} 行舐めた (頭打ちは {ceiling} 行)",
            v.scanned_lines
        );
        // ファイル全体を舐めていないこと (ここが本題)。
        assert!(
            v.scanned_lines * 4 < first,
            "先頭から舐め直している: {} 行 / 開始行 {first}",
            v.scanned_lines
        );
    }

    #[test]
    fn 文書を二倍にしても可視域の塗りに要る行数は増えない() {
        let one = 行を跨ぐ構文を含む合成ソース();
        let two = format!("{one}{one}");
        let n = line_starts(&one).len() - 1;
        let first = (n / 2) - (n / 2) % WINDOW_BLOCK_LINES;
        let h = Highlighter::new();

        let mut got = Vec::new();
        for text in [&one, &two] {
            reset();
            pump(&h, text, "Rust", first, 50);
            let v = paint(&h, text, "Rust", first, 50);
            assert!(v.exact);
            got.push(v.scanned_lines);
        }
        assert_eq!(
            got[0], got[1],
            "文書を 2 倍にしたら舐める行数が {} → {} に増えた",
            got[0], got[1]
        );
    }

    #[test]
    fn 可視域より後ろを直しても足場は生き残る() {
        let text = 行を跨ぐ構文を含む合成ソース();
        let starts = line_starts(&text);
        let n = starts.len() - 1;
        let first = (n / 3) - (n / 3) % WINDOW_BLOCK_LINES;
        let h = Highlighter::new();
        reset();
        pump(&h, &text, "Rust", first, 50);
        let before = paint(&h, &text, "Rust", first, 50);
        assert!(before.exact);

        // 可視域より**後ろ**の行を書き換える。
        let tail = starts[n - 2];
        let edited = format!("{}// 後ろを直した\n", &text[..tail]);
        let after = paint(&h, &edited, "Rust", first, 50);
        assert!(after.exact, "後ろを直しただけで足場を捨てている");
        assert_eq!(
            after.scanned_lines, before.scanned_lines,
            "後ろの編集で先頭から舐め直している"
        );

        // 逆に**前**を直したら足場は捨てる (捨てないと色がずれる)。
        let edited2 = format!("/* 先頭に開いたコメント\n{text}");
        let after2 = paint(&h, &edited2, "Rust", first, 50);
        assert!(!after2.exact, "前を直したのに古い足場を使っている");
    }

    #[test]
    fn 巨大な実ソースでも再開した色が先頭から通した色と一致する() {
        // 合成ソースだけでなく**実物**でも突き合わせる。深追いすると
        // debug ビルドで分単位になるので、可視域は手前のブロックに置く。
        let text = 巨大な実ソース();
        let starts = line_starts(&text);
        let n = starts.len() - 1;
        let first = 4 * WINDOW_BLOCK_LINES;
        assert!(n > first + WINDOW_BLOCK_LINES, "実ソースが短すぎる");
        let h = Highlighter::new();
        let Window { start: w0, end: w1 } = snap_window(first, 50);
        let (w0, w1) = (w0.min(n), w1.min(n));

        // 先頭から通した答え。syntect の状態は前の行にしか依存しないので、
        // 可視域の末尾までの**前半分**を塗れば同じ答えになる。
        let head = &text[..starts[w1]];
        let head_lines: Vec<&str> = LinesWithEndings::from(head).collect();
        let (set, syntax) = h.syntax_for("Rust").expect("Rust の構文がある");
        let theme_ref = h.ts().themes.get(theme()).expect("既定テーマがある");
        reset();
        let want = highlight_doc(set, syntax, theme_ref, head, Color32::WHITE, 0, None);
        let want_job = job_from_spans(head, &head_lines, &want.spans, &font());

        reset();
        pump(&h, &text, "Rust", first, 50);
        let got = paint(&h, &text, "Rust", first, 50);
        assert!(got.exact, "追い付いたのに暫定表示のまま");
        assert_covered(&got.job);
        let range = starts[w0]..starts[w1];
        let diff = colors_of(&got.job, range.clone())
            .into_iter()
            .zip(colors_of(&want_job, range))
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(diff, 0, "実ソースで {diff} バイトぶん色がずれている");
    }

    #[test]
    fn 追い付き済みなら本文を舐め直さない() {
        // アイドル時の費用はゼロ。スクロールが止まっているあいだ、毎フレーム
        // 呼ばれても syntect には 1 行も通さない。
        let text = 行を跨ぐ構文を含む合成ソース();
        let n = line_starts(&text).len() - 1;
        let first = (n / 4) - (n / 4) % WINDOW_BLOCK_LINES;
        let h = Highlighter::new();
        reset();
        pump(&h, &text, "Rust", first, 50);
        let win = snap_window(first, 50);
        for i in 0..8 {
            let a = h.advance_to_visible(&text, "Rust", theme(), Color32::WHITE, win);
            assert!(a.ready, "{i} 回目で追い付きが外れた");
            assert_eq!(a.scanned_lines, 0, "{i} 回目に舐め直している");
        }
    }

    #[test]
    fn 間引いても等間隔が保たれる() {
        let h = Highlighter::new();
        let (_, syntax) = h.syntax_for("Rust").expect("Rust の構文がある");
        let theme_ref = h.ts().themes.get(theme()).expect("既定テーマがある");
        let thl = ThemeHighlighter::new(theme_ref);
        let mut wc = WinCache::new(0, syntax, &thl);
        // 実際の解析は要らないので、スナップショットだけを積む。
        let zero = wc.points[0].clone();
        // `win_advance` と同じ規則で積む: いまの間隔の倍数に達したら 1 つ置く。
        for line in 1..=MAX_CHECKPOINTS * CHECKPOINT_LINES * 4 {
            if !line.is_multiple_of(wc.stride) {
                continue;
            }
            let mut p = zero.clone();
            p.cp.line = line;
            wc.points.push(p);
            wc.thin();
        }
        assert!(wc.points.len() <= MAX_CHECKPOINTS, "上限を超えて抱えている");
        assert!(wc.stride > CHECKPOINT_LINES, "間隔が広がっていない");
        let gaps: BTreeSet<usize> = wc
            .points
            .windows(2)
            .map(|w| w[1].cp.line - w[0].cp.line)
            .collect();
        assert_eq!(gaps.len(), 1, "等間隔でない: {gaps:?}");
        assert_eq!(*gaps.iter().next().expect("1 つある"), wc.stride);
    }
}
