//! 🧬 追記の自動マージ — 「一覧への追記」を git の衝突にしない merge driver。
//!
//! ## なぜ要るのか (このリポジトリで実際に起きたこと)
//!
//! 機能追加の共有面は `src/features/` のレジストリでほぼゼロになったが、
//! CLAUDE.md が正直に認めているとおり **2 つだけ残っている**:
//! `src/config.rs` の設定一覧と `src/keybinds.rs` の `BindAction` 配列。
//! どちらも「独立した追記を**両方**残すだけ」の自明な解決で済むのに、
//! git は「同じファイルの近い行を 2 つのブランチが触った」というだけで
//! 衝突にする (which-key と local_history が実際に 3 ハンク衝突した)。
//!
//! そしてこれは zaivern 固有の話ではない。**どのリポジトリにも**一覧・表・
//! 登録簿への追記はある — `mod` 宣言・i18n テーブル・ルート表・CHANGELOG・
//! `Cargo.toml` の依存・`package.json`・`.gitignore`・エクスポート一覧。
//! 並列エージェント開発の衝突はここに集中する。
//!
//! ## 何をするのか
//!
//! git の [custom merge driver] として動く。`%O`(base) `%A`(ours) `%B`(theirs)
//! を受け取り、**両側が「追記しかしていない」領域だけ**を両方残して解決する。
//!
//! ## 安全側に倒す (初日にオフにされないための設計)
//!
//! * 片側でも既存行を**変更・削除**していたら、その領域は解決せず
//!   [`Resolution::Conflict`] として人間に返す。
//! * 既定では `zaivern:union-begin` / `zaivern:union-end` を含む行で
//!   囲まれた**領域の内側だけ**を対象にする。コメント記号を見ないので
//!   Rust / JS / Python / TOML / YAML / Markdown どれでも同じ書き方で効く。
//! * **マーカが 1 つも無いファイルでは何もしない。** [`cli_main`] は
//!   `git merge-file` へそのまま委譲するので、**素の git と 1 バイトも
//!   変わらない**結果になる。「入れたら普段のマージが変わった」が起きない。
//! * 順序は決定的 (ours → theirs)。`HashMap` / `HashSet` を使わず
//!   `Vec` と `BTreeSet` だけで組んであるので、反復順が出力へ漏れない。
//!
//! ## 使い方
//!
//! ```text
//! # 一覧の周りをマーカで囲む (コメント記号は何でもよい)
//! // zaivern:union-begin
//! pub const A: u8 = 1;
//! pub const B: u8 = 2;
//! // zaivern:union-end
//! ```
//!
//! パレットの「🧬 追記の自動マージ」から導入すると、リポジトリのローカル
//! 設定へ 3 つのドライバが登録される:
//!
//! | 名前 | 対象 |
//! |---|---|
//! | `zaivern-union` | マーカの内側だけ |
//! | `zaivern-union-whole` | ファイル全体 (`CHANGELOG.md` のような純粋な一覧) |
//! | `zaivern-union-sorted` | ファイル全体 + 追記を辞書順に整列 |
//!
//! [custom merge driver]: https://git-scm.com/docs/gitattributes#_defining_a_custom_merge_driver

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::i18n::{tr, trf};
use crate::worktree::git_out;

// ═══════════════════════════════════════════════════════════════════════
//  1. 領域マーカと定数
// ═══════════════════════════════════════════════════════════════════════

/// 領域の開始を表す印。**行にこの文字列が含まれていればよい** (コメント記号は問わない)。
pub const BEGIN: &str = "zaivern:union-begin";
/// 領域の終了を表す印。
pub const END: &str = "zaivern:union-end";

/// `.gitattributes` に書き込む管理ブロックの開始行。
///
/// `union-managed-begin` であって [`BEGIN`] (`union-begin`) ではない —
/// `.gitattributes` 自身が誤って領域として扱われないようにわざと外してある。
const ATTR_BEGIN: &str = "# zaivern:union-managed-begin — Zaivern が管理します (行を足すならこのブロックの外へ)";
/// `.gitattributes` の管理ブロックの終了行。
const ATTR_END: &str = "# zaivern:union-managed-end";
/// 管理ブロックを探すときの目印 (説明文が変わっても拾えるように前方一致で見る)。
const ATTR_BEGIN_KEY: &str = "# zaivern:union-managed-begin";
const ATTR_END_KEY: &str = "# zaivern:union-managed-end";

/// 登録するドライバ名と、それに渡す追加フラグ。
///
/// **オプションをドライバ名に埋めてある**のが肝で、こうすると
/// [`cli_main`] が設定ファイル (= 実ホーム) を 1 度も読まずに済む。
/// マージの最中に `~/.zaivern` を読みに行く設計は、テストでも本番でも
/// 事故のもとになる。
const DRIVERS: &[(&str, &str)] = &[
    ("zaivern-union", ""),
    ("zaivern-union-whole", "--whole"),
    ("zaivern-union-sorted", "--whole --sorted"),
];

/// `git config merge.<名前>.name` に書く説明。
const DRIVER_DESC: &str = "Zaivern: 追記どうしの衝突だけを自動で解決する";

/// `.gitattributes` へ書く既定のパターン。設定 `union.patterns` で変えられる。
const DEFAULT_PATTERNS: &str = "*.md *.toml *.txt *.json *.yaml *.yml";

/// LCS を素の DP で解いてよい上限 (セル数)。超えたら一意行アンカーで分割する。
const DP_CELLS: usize = 1_000_000;

/// 衝突マーカの既定の長さ (git の既定と同じ)。
const DEFAULT_MARKER_SIZE: usize = 7;

// ═══════════════════════════════════════════════════════════════════════
//  2. 行と改行コード — 改行は「行の持ち物」として運ぶ
// ═══════════════════════════════════════════════════════════════════════

/// 1 行 = 本文 + その行の改行。
///
/// 改行を行に持たせるのは **CRLF / LF の混在をそのまま保つ**ため。
/// 比較は [`Line::text`] だけで行うので、片側が CRLF へ変換されていても
/// 「全行が書き換わった」とは誤認しない。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Line {
    text: String,
    /// `"\n"` / `"\r\n"` / `""` (末尾に改行が無い最終行)。
    eol: &'static str,
}

/// 文字列を [`Line`] へ割る。**改行の種類と末尾改行の有無を落とさない。**
fn split_lines(s: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find('\n') {
        let (head, tail) = rest.split_at(i);
        let (text, eol) = match head.strip_suffix('\r') {
            Some(h) => (h, "\r\n"),
            None => (head, "\n"),
        };
        out.push(Line {
            text: text.to_string(),
            eol,
        });
        rest = &tail[1..];
    }
    if !rest.is_empty() {
        out.push(Line {
            text: rest.to_string(),
            eol: "",
        });
    }
    out
}

/// [`Line`] を繋いで文字列へ戻す。
///
/// 「改行なし」の行が途中に来てしまった場合 (元は最終行だったが、後ろへ
/// 追記された) だけ、`dominant` を補う。**最終行の改行の有無はそのまま。**
fn join_lines(lines: &[Line], dominant: &'static str) -> String {
    let mut s = String::new();
    for (i, l) in lines.iter().enumerate() {
        s.push_str(&l.text);
        if l.eol.is_empty() && i + 1 < lines.len() {
            s.push_str(dominant);
        } else {
            s.push_str(l.eol);
        }
    }
    s
}

/// その版で多数派の改行。1 行も改行を持たなければ `None`。
fn eol_of(lines: &[Line]) -> Option<&'static str> {
    let (mut crlf, mut lf) = (0usize, 0usize);
    for l in lines {
        match l.eol {
            "\r\n" => crlf += 1,
            "\n" => lf += 1,
            _ => {}
        }
    }
    if crlf == 0 && lf == 0 {
        None
    } else if crlf > lf {
        Some("\r\n")
    } else {
        Some("\n")
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  3. 行の差分 — 共通接頭辞/接尾辞 → DP → 一意行アンカー
// ═══════════════════════════════════════════════════════════════════════

/// `a` と `b` の対応表 (共通行のインデックス対)。**両方について単調増加**。
fn diff_pairs(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    diff_rec(a, b, 0, 0, &mut out, 0);
    out
}

fn diff_rec(a: &[&str], b: &[&str], ao: usize, bo: usize, out: &mut Vec<(usize, usize)>, depth: u32) {
    // 共通接頭辞 — 実測上ほとんどの追記はここだけで片付く。
    let mut p = 0;
    while p < a.len() && p < b.len() && a[p] == b[p] {
        out.push((ao + p, bo + p));
        p += 1;
    }
    let (a, b, ao, bo) = (&a[p..], &b[p..], ao + p, bo + p);
    // 共通接尾辞。接頭辞を削った時点でどちらかは空か先頭が違うので、重複しない。
    let mut q = 0;
    while q < a.len().min(b.len()) && a[a.len() - 1 - q] == b[b.len() - 1 - q] {
        q += 1;
    }
    let mid_a = &a[..a.len() - q];
    let mid_b = &b[..b.len() - q];
    if !mid_a.is_empty() && !mid_b.is_empty() {
        if mid_a.len().saturating_mul(mid_b.len()) <= DP_CELLS {
            lcs_dp(mid_a, mid_b, ao, bo, out);
        } else if depth < 32 {
            // 巨大な塊は「両側に 1 回だけ出てくる行」を足場にして割る
            // (patience diff と同じ手)。足場が無ければ丸ごと置換として扱う。
            let anchors = unique_anchors(mid_a, mid_b);
            let (mut pa, mut pb) = (0usize, 0usize);
            for (x, y) in anchors {
                diff_rec(&mid_a[pa..x], &mid_b[pb..y], ao + pa, bo + pb, out, depth + 1);
                out.push((ao + x, bo + y));
                pa = x + 1;
                pb = y + 1;
            }
            if pa > 0 || pb > 0 {
                diff_rec(&mid_a[pa..], &mid_b[pb..], ao + pa, bo + pb, out, depth + 1);
            }
        }
    }
    for k in (0..q).rev() {
        out.push((ao + a.len() - 1 - k, bo + b.len() - 1 - k));
    }
}

/// 素の LCS (動的計画法)。呼び出し側がセル数を上限で抑えている。
fn lcs_dp(a: &[&str], b: &[&str], ao: usize, bo: usize, out: &mut Vec<(usize, usize)>) {
    let (n, m) = (a.len(), b.len());
    let w = m + 1;
    let mut dp = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * w + j] = if a[i] == b[j] {
                dp[(i + 1) * w + j + 1] + 1
            } else {
                dp[(i + 1) * w + j].max(dp[i * w + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((ao + i, bo + j));
            i += 1;
            j += 1;
        } else if dp[(i + 1) * w + j] >= dp[i * w + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
}

/// 「両側にちょうど 1 回だけ出てくる行」のうち、順序が保たれる最大の組。
fn unique_anchors(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    let mut ca: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for (i, s) in a.iter().enumerate() {
        let e = ca.entry(s).or_insert((0, 0));
        e.0 += 1;
        e.1 = i;
    }
    let mut cb: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for (i, s) in b.iter().enumerate() {
        let e = cb.entry(s).or_insert((0, 0));
        e.0 += 1;
        e.1 = i;
    }
    let mut cand: Vec<(usize, usize)> = Vec::new();
    for (s, (n, ia)) in &ca {
        if *n != 1 {
            continue;
        }
        if let Some((m, ib)) = cb.get(s) {
            if *m == 1 {
                cand.push((*ia, *ib));
            }
        }
    }
    cand.sort_unstable();
    lis_by_second(&cand)
}

/// 2 つ目の値についての最長増加部分列 (patience の LIS 段)。
fn lis_by_second(cand: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut tails: Vec<usize> = Vec::new();
    let mut prev: Vec<Option<usize>> = vec![None; cand.len()];
    for (k, &(_, y)) in cand.iter().enumerate() {
        let pos = tails.partition_point(|&t| cand[t].1 < y);
        if pos > 0 {
            prev[k] = Some(tails[pos - 1]);
        }
        if pos == tails.len() {
            tails.push(k);
        } else {
            tails[pos] = k;
        }
    }
    let mut out = Vec::new();
    let mut cur = tails.last().copied();
    while let Some(k) = cur {
        out.push(cand[k]);
        cur = prev[k];
    }
    out.reverse();
    out
}

// ═══════════════════════════════════════════════════════════════════════
//  4. 領域 — どの行が union の対象か
// ═══════════════════════════════════════════════════════════════════════

/// 1 つの版について「どの行/どの隙間が領域の内側か」を持つ表。
struct Regions {
    /// 隙間 `p` (行 `p-1` と行 `p` の間) が属する領域。長さは `行数 + 1`。
    gap: Vec<Option<usize>>,
    /// その行がマーカ行そのものか。**マーカ行は絶対に動かさない。**
    marker: Vec<bool>,
    /// 領域の数。
    count: usize,
    /// マーカの開閉が揃っているか。**揃っていなければ union は一切適用しない**
    /// (途中で切れたファイルや、衝突マーカが残ったファイルを掴まない)。
    balanced: bool,
}

/// 領域表を作る。**マーカが 1 つでもあれば `whole_file` より markers が優先**
/// (明示的に囲った意図を、ファイル単位の指定で上書きしないため)。
fn regions_of(lines: &[Line], whole_file: bool) -> Regions {
    let n = lines.len();
    let mut marker = vec![false; n];
    let mut gap = vec![None; n + 1];
    let mut cur: Option<usize> = None;
    let mut count = 0usize;
    let mut balanced = true;
    for (i, l) in lines.iter().enumerate() {
        gap[i] = cur;
        if l.text.contains(BEGIN) {
            marker[i] = true;
            // 入れ子は許さない。開いたまま次を開いたら「揃っていない」。
            if cur.is_some() {
                balanced = false;
            }
            cur = Some(count);
            count += 1;
        } else if l.text.contains(END) {
            marker[i] = true;
            if cur.is_none() {
                balanced = false;
            }
            cur = None;
        }
    }
    gap[n] = cur;
    if cur.is_some() {
        balanced = false; // 閉じていない
    }
    if count == 0 && whole_file {
        return Regions {
            gap: vec![Some(0); n + 1],
            marker: vec![false; n],
            count: 1,
            balanced: true,
        };
    }
    Regions {
        gap,
        marker,
        count,
        balanced,
    }
}

/// `start..start+len` が丸ごと 1 つの領域の内側なら、その領域番号。
fn slice_region(r: &Regions, start: usize, len: usize) -> Option<usize> {
    let g0 = r.gap[start]?;
    let g1 = r.gap[start + len]?;
    if g0 != g1 {
        return None;
    }
    if r.marker[start..start + len].iter().any(|m| *m) {
        return None;
    }
    Some(g0)
}

// ═══════════════════════════════════════════════════════════════════════
//  5. 公開 API
// ═══════════════════════════════════════════════════════════════════════

/// [`resolve`] の振る舞いを決める設定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnionOpts {
    /// マーカが 1 つも無くてもファイル全体を対象にする。**既定は off**。
    /// `.gitattributes` で `merge=zaivern-union-whole` を付けたときだけ立つ。
    pub whole_file: bool,
    /// 追記を辞書順に整列する。並び順に意味が無い一覧向け。
    pub sorted: bool,
    /// 衝突マーカの長さ (git が `%L` で渡してくる)。
    pub marker_size: usize,
    /// 衝突マーカの ours 側ラベル。
    pub ours_label: String,
    /// 衝突マーカの theirs 側ラベル。
    pub theirs_label: String,
}

impl Default for UnionOpts {
    fn default() -> Self {
        Self {
            whole_file: false,
            sorted: false,
            marker_size: DEFAULT_MARKER_SIZE,
            ours_label: "ours".into(),
            theirs_label: "theirs".into(),
        }
    }
}

/// マージの結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// 完全に解決した。中身はそのまま書き出せる。
    Merged(String),
    /// 衝突が残った。中身には **git 標準の衝突マーカ**が入っている。
    Conflict(String),
}

impl Resolution {
    /// 結果の本文 (解決済みでも衝突入りでも、書き出す中身はこれ)。
    pub fn text(&self) -> &str {
        match self {
            Resolution::Merged(s) | Resolution::Conflict(s) => s,
        }
    }
    /// 衝突が残っているか。git のドライバは残っていれば非 0 で返す。
    pub fn has_conflict(&self) -> bool {
        matches!(self, Resolution::Conflict(_))
    }
}

/// 3-way マージ。**両側が追記しかしていない領域だけ**を両方残す。
///
/// 片側でも既存行を変更・削除していたら、その領域は解決せず
/// [`Resolution::Conflict`] を返す (誤って解決するより人間に返す)。
pub fn resolve(base: &str, ours: &str, theirs: &str, opts: &UnionOpts) -> Resolution {
    let b = split_lines(base);
    let o = split_lines(ours);
    let t = split_lines(theirs);
    let dom = eol_of(&o)
        .or_else(|| eol_of(&t))
        .or_else(|| eol_of(&b))
        .unwrap_or("\n");
    let (lines, conflicts) = three_way(&b, &o, &t, opts, dom);
    let text = join_lines(&lines, dom);
    if conflicts == 0 {
        Resolution::Merged(text)
    } else {
        Resolution::Conflict(text)
    }
}

/// この 3 つの版のどれかに領域マーカがあるか。
///
/// **1 つも無ければ [`cli_main`] は `git merge-file` へ丸投げする** ので、
/// 素の git と 1 バイトも変わらない結果になる。
pub fn has_marker(base: &str, ours: &str, theirs: &str) -> bool {
    [base, ours, theirs]
        .iter()
        .any(|s| s.contains(BEGIN) || s.contains(END))
}

// ═══════════════════════════════════════════════════════════════════════
//  6. 3-way マージ本体
// ═══════════════════════════════════════════════════════════════════════

/// マージ結果の行と、残った衝突の数。
fn three_way(
    base: &[Line],
    ours: &[Line],
    theirs: &[Line],
    opts: &UnionOpts,
    dom: &'static str,
) -> (Vec<Line>, usize) {
    let bt: Vec<&str> = base.iter().map(|l| l.text.as_str()).collect();
    let ot: Vec<&str> = ours.iter().map(|l| l.text.as_str()).collect();
    let tt: Vec<&str> = theirs.iter().map(|l| l.text.as_str()).collect();

    let mut o_of: Vec<Option<usize>> = vec![None; base.len()];
    for (i, j) in diff_pairs(&bt, &ot) {
        o_of[i] = Some(j);
    }
    let mut t_of: Vec<Option<usize>> = vec![None; base.len()];
    for (i, j) in diff_pairs(&bt, &tt) {
        t_of[i] = Some(j);
    }

    let rb = regions_of(base, opts.whole_file);
    let ro = regions_of(ours, opts.whole_file);
    let rt = regions_of(theirs, opts.whole_file);
    // **片側でもマーカを触っていたら union は一切効かせない。**
    let union_ok = rb.balanced
        && ro.balanced
        && rt.balanced
        && rb.count > 0
        && rb.count == ro.count
        && ro.count == rt.count;

    let mut out: Vec<Line> = Vec::new();
    let mut conflicts = 0usize;
    let (mut pb, mut po, mut pt) = (0usize, 0usize, 0usize);
    for i in 0..base.len() {
        let (Some(oj), Some(tj)) = (o_of[i], t_of[i]) else {
            continue;
        };
        emit_chunk(
            Slices {
                base: &base[pb..i],
                ours: &ours[po..oj],
                theirs: &theirs[pt..tj],
                at: (pb, po, pt),
            },
            &Ctx {
                rb: &rb,
                ro: &ro,
                rt: &rt,
                union_ok,
                opts,
                dom,
            },
            &mut out,
            &mut conflicts,
        );
        // 同期点そのものは ours 側の行を採る (改行コードの主は ours)。
        out.push(ours[oj].clone());
        pb = i + 1;
        po = oj + 1;
        pt = tj + 1;
    }
    emit_chunk(
        Slices {
            base: &base[pb..],
            ours: &ours[po..],
            theirs: &theirs[pt..],
            at: (pb, po, pt),
        },
        &Ctx {
            rb: &rb,
            ro: &ro,
            rt: &rt,
            union_ok,
            opts,
            dom,
        },
        &mut out,
        &mut conflicts,
    );
    (out, conflicts)
}

/// 1 チャンクぶんの 3 つの断片と、それぞれの開始位置。
struct Slices<'a> {
    base: &'a [Line],
    ours: &'a [Line],
    theirs: &'a [Line],
    at: (usize, usize, usize),
}

/// チャンク処理に要る共通の材料 (引数の数を増やさないための束)。
struct Ctx<'a> {
    rb: &'a Regions,
    ro: &'a Regions,
    rt: &'a Regions,
    union_ok: bool,
    opts: &'a UnionOpts,
    dom: &'static str,
}

fn same(a: &[Line], b: &[Line]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.text == y.text)
}

fn emit_chunk(s: Slices<'_>, cx: &Ctx<'_>, out: &mut Vec<Line>, conflicts: &mut usize) {
    let (mut b, mut o, mut t) = (s.base, s.ours, s.theirs);
    let (mut ab, mut ao, mut at) = s.at;
    if b.is_empty() && o.is_empty() && t.is_empty() {
        return;
    }
    // 三者が揃って同じ先頭・末尾はチャンクの外へ出す。衝突を出すときの
    // 巻き込みが減り、領域判定も正確になる。
    while !b.is_empty()
        && !o.is_empty()
        && !t.is_empty()
        && b[0].text == o[0].text
        && b[0].text == t[0].text
    {
        out.push(o[0].clone());
        b = &b[1..];
        o = &o[1..];
        t = &t[1..];
        ab += 1;
        ao += 1;
        at += 1;
    }
    let mut tail: Vec<Line> = Vec::new();
    while !b.is_empty()
        && !o.is_empty()
        && !t.is_empty()
        && b[b.len() - 1].text == o[o.len() - 1].text
        && b[b.len() - 1].text == t[t.len() - 1].text
    {
        tail.push(o[o.len() - 1].clone());
        b = &b[..b.len() - 1];
        o = &o[..o.len() - 1];
        t = &t[..t.len() - 1];
    }
    tail.reverse();

    if !(b.is_empty() && o.is_empty() && t.is_empty()) {
        if same(o, b) {
            out.extend(t.iter().cloned()); // theirs だけが変えた
        } else if same(t, b) {
            out.extend(o.iter().cloned()); // ours だけが変えた
        } else if same(o, t) {
            out.extend(o.iter().cloned()); // 両側が同じ変更をした
        } else {
            let merged = if cx.union_ok
                && slice_region(cx.rb, ab, b.len()).is_some()
                && slice_region(cx.ro, ao, o.len()).is_some()
                && slice_region(cx.rt, at, t.len()).is_some()
                && slice_region(cx.rb, ab, b.len()) == slice_region(cx.ro, ao, o.len())
                && slice_region(cx.ro, ao, o.len()) == slice_region(cx.rt, at, t.len())
            {
                union_merge(b, o, t, cx.opts)
            } else {
                None
            };
            match merged {
                Some(lines) => out.extend(lines),
                None => {
                    *conflicts += 1;
                    out.extend(conflict_lines(o, t, cx.opts, cx.dom));
                }
            }
        }
    }
    out.extend(tail);
}

/// 「両側とも追記しかしていない」なら両方残した行を返す。そうでなければ `None`。
fn union_merge(b: &[Line], o: &[Line], t: &[Line], opts: &UnionOpts) -> Option<Vec<Line>> {
    let (o_ins, o_anchor) = align(b, o)?;
    let (t_ins, _) = align(b, t)?;
    // 両ブランチが**同じ行**を足した場合は 1 本にまとめる。空白だけの行は
    // 区切りとして何度も出てくるのが普通なので、畳まない。
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for slot in &o_ins {
        for l in slot {
            if !l.text.trim().is_empty() {
                seen.insert(l.text.as_str());
            }
        }
    }
    let mut out: Vec<Line> = Vec::new();
    for k in 0..=b.len() {
        let mut block: Vec<Line> = o_ins[k].clone();
        for l in &t_ins[k] {
            if !l.text.trim().is_empty() && seen.contains(l.text.as_str()) {
                continue;
            }
            block.push(l.clone());
        }
        if opts.sorted {
            block.sort_by(|x, y| x.text.cmp(&y.text));
        }
        out.extend(block);
        if k < b.len() {
            out.push(o_anchor[k].clone());
        }
    }
    Some(out)
}

/// `side` が `b` に**追記しただけ**なら、隙間ごとの追記と足場の行を返す。
///
/// 既存行が 1 行でも消えている / 書き換わっていれば `None`
/// (= 自動では解決しない)。最左貪欲は部分列判定として最適なので、
/// 「追記だけ」なら必ず見つかる。
fn align(b: &[Line], side: &[Line]) -> Option<(Vec<Vec<Line>>, Vec<Line>)> {
    let mut ins: Vec<Vec<Line>> = vec![Vec::new(); b.len() + 1];
    let mut anchor: Vec<Line> = Vec::with_capacity(b.len());
    let mut j = 0usize;
    for l in side {
        if j < b.len() && l.text == b[j].text {
            anchor.push(l.clone());
            j += 1;
        } else {
            ins[j].push(l.clone());
        }
    }
    if j == b.len() {
        Some((ins, anchor))
    } else {
        None
    }
}

/// git 標準の衝突マーカ付きの行を作る。
fn conflict_lines(o: &[Line], t: &[Line], opts: &UnionOpts, dom: &'static str) -> Vec<Line> {
    let n = opts.marker_size.clamp(1, 200);
    let mk = |c: char, label: &str| Line {
        text: if label.is_empty() {
            c.to_string().repeat(n)
        } else {
            format!("{} {label}", c.to_string().repeat(n))
        },
        eol: dom,
    };
    let mut out = Vec::new();
    out.push(mk('<', &opts.ours_label));
    out.extend(o.iter().cloned());
    out.push(mk('=', ""));
    out.extend(t.iter().cloned());
    out.push(mk('>', &opts.theirs_label));
    out
}

// ═══════════════════════════════════════════════════════════════════════
//  7. git マージドライバの入口
// ═══════════════════════════════════════════════════════════════════════

/// `zai merge-driver %O %A %B %L %P` の実体。argv は `"merge-driver"` の**次**から。
///
/// git の規約どおり、結果は `%A`(ours) のパスへ**上書き**し、
/// 完全解決なら 0、衝突が残るなら非 0 を返す。
///
/// * `%O` = base の一時ファイル
/// * `%A` = ours の一時ファイル (**ここへ書く**)
/// * `%B` = theirs の一時ファイル
/// * `%L` = 衝突マーカの長さ
/// * `%P` = 元のパス (表示にだけ使う)
pub fn cli_main(argv: &[String]) -> i32 {
    let mut opts = UnionOpts::default();
    let mut pos: Vec<&str> = Vec::new();
    for a in argv {
        match a.as_str() {
            "--whole" | "--whole-file" => opts.whole_file = true,
            "--sorted" => opts.sorted = true,
            "-h" | "--help" => {
                println!("{}", driver_help());
                return 0;
            }
            other => pos.push(other),
        }
    }
    if pos.len() < 3 {
        eprintln!("{}", driver_help());
        return 2;
    }
    let (o, a, b) = (Path::new(pos[0]), Path::new(pos[1]), Path::new(pos[2]));
    opts.marker_size = pos
        .get(3)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MARKER_SIZE)
        .clamp(1, 200);
    let shown = pos.get(4).copied().unwrap_or("");

    let (Ok(bb), Ok(ab), Ok(tb)) = (std::fs::read(o), std::fs::read(a), std::fs::read(b)) else {
        eprintln!("{}", tr("zaivern-union: 一時ファイルを読めませんでした。"));
        return 2;
    };
    // UTF-8 でないものはこちらでは扱わない。git 本体の実装へそのまま渡す。
    let (Ok(bs), Ok(os), Ok(ts)) = (
        String::from_utf8(bb),
        String::from_utf8(ab),
        String::from_utf8(tb),
    ) else {
        return delegate_to_git(o, a, b, opts.marker_size);
    };
    // **マーカが 1 つも無いファイルでは何もしない。** git 本体へ委譲するので、
    // 「ドライバを入れたら普段のマージまで変わった」が構造的に起こらない。
    if !opts.whole_file && !has_marker(&bs, &os, &ts) {
        return delegate_to_git(o, a, b, opts.marker_size);
    }

    let res = resolve(&bs, &os, &ts, &opts);
    if std::fs::write(a, res.text().as_bytes()).is_err() {
        eprintln!("{}", tr("zaivern-union: 結果を書き出せませんでした。"));
        return 2;
    }
    if res.has_conflict() {
        if !shown.is_empty() {
            eprintln!(
                "{}",
                trf(
                    "zaivern-union: {p} は自動で解決できない変更を含みます。",
                    &[("p", shown.to_string())]
                )
            );
        }
        1
    } else {
        0
    }
}

/// git 本体の 3-way マージへ委譲する (`git merge-file` は `%A` を上書きする)。
fn delegate_to_git(o: &Path, a: &Path, b: &Path, marker: usize) -> i32 {
    let st = crate::procx::hidden_command("git")
        .arg("merge-file")
        .arg(format!("--marker-size={marker}"))
        .args(["-L", "ours", "-L", "base", "-L", "theirs"])
        .arg(a)
        .arg(o)
        .arg(b)
        .status();
    match st {
        // git merge-file は「残った衝突の数」を返す (>=128 は異常終了)。
        Ok(s) => s.code().unwrap_or(1).clamp(0, 127),
        Err(_) => 1,
    }
}

fn driver_help() -> String {
    tr("\
使い方: zai merge-driver [--whole] [--sorted] <base> <ours> <theirs> [マーカ長] [元のパス]

git の custom merge driver です。手で呼ぶものではありません
(パレットの「🧬 追記の自動マージ」から導入してください)。
結果は <ours> のパスへ上書きし、衝突が残ったときだけ 1 を返します。")
}

// ═══════════════════════════════════════════════════════════════════════
//  8. 導入 / 解除
// ═══════════════════════════════════════════════════════════════════════

/// `sh -c` に渡しても壊れない引用。git はドライバをシェル経由で起動する。
///
/// Windows では区切りを `/` へ倒す (git が同梱する sh は `\` を
/// エスケープとして解釈するため)。**パスの直書きは 1 つも無い。**
fn sh_quote(p: &Path) -> String {
    let raw = p.to_string_lossy().into_owned();
    let raw = if cfg!(windows) {
        raw.replace('\\', "/")
    } else {
        raw
    };
    format!("'{}'", raw.replace('\'', r"'\''"))
}

/// 登録するドライバのコマンド行。実行ファイルの場所は
/// [`std::env::current_exe`] から導出する (ハードコード禁止)。
fn driver_command(exe: &Path, flags: &str) -> String {
    let q = sh_quote(exe);
    if flags.is_empty() {
        format!("{q} merge-driver %O %A %B %L %P")
    } else {
        format!("{q} merge-driver {flags} %O %A %B %L %P")
    }
}

/// このリポジトリへドライバを登録し、`.gitattributes` の管理ブロックを書く。
pub fn install(repo: &Path) -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|e| trf("実行ファイルの場所が分かりません: {e}", &[("e", e.to_string())]))?;
    install_with(repo, &exe, &patterns_from_config(repo))
}

/// [`install`] の中身。実行ファイルとパターンを外から渡せる形
/// (テストがビルド済みバイナリの場所を差し替えられるようにするため)。
pub fn install_with(repo: &Path, exe: &Path, patterns: &[String]) -> Result<String, String> {
    let root = crate::worktree::repo_root(repo)?;
    for (name, flags) in DRIVERS {
        let key_name = format!("merge.{name}.name");
        let key_drv = format!("merge.{name}.driver");
        let cmd = driver_command(exe, flags);
        git_out(&root, &["config", "--local", &key_name, DRIVER_DESC])?;
        git_out(&root, &["config", "--local", &key_drv, &cmd])?;
    }
    write_attributes(&root, patterns)?;
    Ok(trf(
        "追記の自動マージを導入しました ({n} パターン)。",
        &[("n", patterns.len().to_string())],
    ))
}

/// 登録を解除し、`.gitattributes` の管理ブロックを取り除く。
pub fn uninstall(repo: &Path) -> Result<String, String> {
    let root = crate::worktree::repo_root(repo)?;
    for (name, _) in DRIVERS {
        let section = format!("merge.{name}");
        // 未登録でも失敗にしない (解除は何度呼んでも同じ結果にする)。
        let _ = git_out(&root, &["config", "--local", "--remove-section", &section]);
    }
    strip_attributes(&root)?;
    Ok(tr("追記の自動マージを解除しました。"))
}

/// このリポジトリにドライバが登録済みか。
pub fn is_installed(repo: &Path) -> bool {
    git_out(repo, &["config", "--local", "--get", "merge.zaivern-union.driver"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// 設定 `union.patterns` から `.gitattributes` へ書くパターンを起こす。
fn patterns_from_config(repo: &Path) -> Vec<String> {
    let cfg = crate::config::load(std::slice::from_ref(&repo.to_path_buf()), false);
    let raw = cfg.feature_str("union.patterns");
    split_patterns(&raw)
}

/// 空白 / カンマ区切りのパターン列を割る。空なら既定へ戻す。
pub fn split_patterns(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = raw
        .split([' ', '\t', ',', '\n'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    out.dedup();
    if out.is_empty() {
        out = DEFAULT_PATTERNS.split(' ').map(|s| s.to_string()).collect();
    }
    out
}

/// `.gitattributes` から管理ブロックだけを抜く。**既存の行は 1 つも壊さない。**
///
/// 終了行だけ手で消されたブロックでも、**こちらが書いた形の行しか消さない**。
/// 「開始行から末尾まで」を丸ごと捨てると、ブロックの後ろに人が足した行を
/// 巻き込んで消してしまう (`.gitattributes` を触るツールが一番嫌われる壊れ方)。
///
/// 戻り値は `(残った本文, 使われている改行)`。
fn strip_block(old: &str) -> (String, &'static str) {
    let eol = if old.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::new();
    let mut skipping = false;
    for line in old.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if !skipping {
            if trimmed.starts_with(ATTR_BEGIN_KEY) {
                skipping = true;
            } else {
                out.push_str(line);
            }
            continue;
        }
        if trimmed.starts_with(ATTR_END_KEY) {
            skipping = false;
            continue;
        }
        if is_generated_attr_line(trimmed) {
            continue;
        }
        // 見覚えの無い行に当たった = ブロックはここで終わっている。
        skipping = false;
        out.push_str(line);
    }
    (out, eol)
}

/// 管理ブロックの中身としてこちらが書きうる行か (`<パターン> merge=zaivern-union…`)。
fn is_generated_attr_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return true;
    }
    let mut it = t.split_whitespace();
    let (Some(_pattern), Some(attr), None) = (it.next(), it.next(), it.next()) else {
        return false;
    };
    attr.starts_with("merge=zaivern-union")
}

fn write_attributes(root: &Path, patterns: &[String]) -> Result<(), String> {
    let path = root.join(".gitattributes");
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let (mut kept, eol) = strip_block(&old);
    if !kept.is_empty() && !kept.ends_with('\n') {
        kept.push_str(eol);
    }
    kept.push_str(ATTR_BEGIN);
    kept.push_str(eol);
    for p in patterns {
        kept.push_str(&format!("{p} merge=zaivern-union{eol}"));
    }
    kept.push_str(ATTR_END);
    kept.push_str(eol);
    std::fs::write(&path, kept)
        .map_err(|e| trf(".gitattributes を書けません: {e}", &[("e", e.to_string())]))
}

fn strip_attributes(root: &Path) -> Result<(), String> {
    let path = root.join(".gitattributes");
    let Ok(old) = std::fs::read_to_string(&path) else {
        return Ok(()); // 無ければ何もしない
    };
    let (kept, _) = strip_block(&old);
    if kept.trim().is_empty() {
        // 管理ブロックしか無かった = こちらが作ったファイル。消して元へ戻す。
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    std::fs::write(&path, kept)
        .map_err(|e| trf(".gitattributes を書けません: {e}", &[("e", e.to_string())]))
}

// ═══════════════════════════════════════════════════════════════════════
//  9. 収集 (裏のスレッド) — UI スレッドは git を待たない
// ═══════════════════════════════════════════════════════════════════════

/// 走査 1 回ぶんの結果。
#[derive(Clone, Debug, Default)]
struct Snapshot {
    installed: bool,
    files: Vec<Target>,
    /// 走査を打ち切った件数 (黙って切らない)。
    truncated: usize,
    /// 画面へそのまま出す注記 (git が無い / リポジトリでない 等)。
    note: Option<String>,
    cost: Duration,
}

/// ドライバの対象になっているファイル 1 件。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Target {
    path: String,
    driver: String,
    /// ファイル内の領域の数。0 なら「対象だが今は素の git と同じ」。
    regions: usize,
}

/// 一度に `git check-attr` へ渡すパス数 (コマンド行の上限を避ける)。
const ATTR_CHUNK: usize = 128;
/// 走査するファイル数の上限。
const MAX_FILES: usize = 5000;
/// 領域数を数えるために中身を読むファイル数の上限。
const MAX_READ: usize = 300;
/// 再走査の基準間隔 (実測に応じて `git::scan_interval` が伸ばす)。
const SCAN_BASE: Duration = Duration::from_secs(4);

fn scan(repo: &Path) -> Snapshot {
    let t0 = Instant::now();
    let mut snap = Snapshot::default();
    let root = match crate::worktree::repo_root(repo) {
        Ok(r) => r,
        Err(e) => {
            snap.note = Some(e);
            snap.cost = t0.elapsed();
            return snap;
        }
    };
    snap.installed = is_installed(&root);

    let listed = git_out(&root, &["ls-files"]).unwrap_or_default();
    let all: Vec<&str> = listed.lines().filter(|l| !l.is_empty()).collect();
    if all.len() > MAX_FILES {
        snap.truncated = all.len() - MAX_FILES;
    }
    let head = &all[..all.len().min(MAX_FILES)];
    for chunk in head.chunks(ATTR_CHUNK) {
        let mut args: Vec<&str> = vec!["check-attr", "merge", "--"];
        args.extend(chunk.iter().copied());
        let Ok(out) = git_out(&root, &args) else {
            continue;
        };
        for line in out.lines() {
            // 形式: "<path>: merge: <value>"。パスに ": " が入りうるので右から割る。
            let mut it = line.rsplitn(3, ": ");
            let (Some(value), Some(attr), Some(path)) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            if attr != "merge" || !value.starts_with("zaivern-union") {
                continue;
            }
            snap.files.push(Target {
                path: path.to_string(),
                driver: value.to_string(),
                regions: 0,
            });
        }
    }
    snap.files.sort_by(|a, b| a.path.cmp(&b.path));
    for f in snap.files.iter_mut().take(MAX_READ) {
        if let Ok(text) = std::fs::read_to_string(root.join(&f.path)) {
            f.regions = text.lines().filter(|l| l.contains(BEGIN)).count();
        }
    }
    snap.cost = t0.elapsed();
    snap
}

/// GUI が開いているワークスペースのルート。`app.rs` を触らずに済ませるため、
/// 自分自身のインスタンス登録から引く (`semconf` / `lease` と同じ手)。
fn gui_workspace_root() -> PathBuf {
    let me = std::process::id();
    crate::instances::scan_and_prune(&crate::instances::instances_dir())
        .into_iter()
        .find(|e| e.pid == me)
        .and_then(|e| e.workspace_roots.first().map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

// ═══════════════════════════════════════════════════════════════════════
// 10. レイアウト (純粋関数) — どの幅でも見切れないことを表で固定する
// ═══════════════════════════════════════════════════════════════════════

/// 空状態カードの矩形。**利用可能領域の中央**に置く (下や上に取り残さない)。
pub fn empty_card(avail: egui::Rect) -> egui::Rect {
    let w = (avail.width() - 32.0).clamp(0.0, 420.0);
    let h = (avail.height() - 32.0).clamp(0.0, 132.0);
    egui::Rect::from_center_size(avail.center(), egui::vec2(w, h))
}

/// 一覧 1 行の列割り。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowLayout {
    pub path_w: f32,
    pub driver_w: f32,
    pub regions_w: f32,
    /// 狭いときはドライバ名の列を畳む。
    pub show_driver: bool,
}

/// 可用幅から列割りを決める。**列の合計は必ず可用幅に収まる。**
pub fn row_layout(avail_w: f32) -> RowLayout {
    let w = avail_w.max(0.0);
    let gap = 8.0;
    let regions_w = 64.0_f32.min((w * 0.2).max(0.0));
    let show_driver = w >= 420.0;
    let driver_w = if show_driver {
        160.0_f32.min(w * 0.3)
    } else {
        0.0
    };
    let used = regions_w + driver_w + if show_driver { gap * 2.0 } else { gap };
    let path_w = (w - used).max(0.0);
    RowLayout {
        path_w,
        driver_w,
        regions_w,
        show_driver,
    }
}

/// 行の矩形を左から並べる。**互いに重ならず、必ず `row` の中に収まる。**
pub fn row_rects(row: egui::Rect, lay: &RowLayout) -> Vec<egui::Rect> {
    let gap = 8.0;
    let mut out = Vec::new();
    let mut x = row.left();
    let mut push = |w: f32, x: &mut f32| {
        let r = egui::Rect::from_min_max(
            egui::pos2(*x, row.top()),
            egui::pos2((*x + w).min(row.right()), row.bottom()),
        );
        *x = (*x + w + gap).min(row.right());
        out.push(r);
    };
    push(lay.path_w, &mut x);
    if lay.show_driver {
        push(lay.driver_w, &mut x);
    }
    push(lay.regions_w, &mut x);
    out
}

// ═══════════════════════════════════════════════════════════════════════
// 11. パネル — `app.rs` を 1 バイトも触らずにウィンドウを出す
// ═══════════════════════════════════════════════════════════════════════

/// パネルの状態。**ウィンドウより長生きさせる** (設計原則 1) ため、
/// `ZaivernApp` のフィールドではなくモジュール側に置く。
#[derive(Default)]
struct PanelState {
    open: bool,
    root: PathBuf,
    snap: Snapshot,
    pending: Option<Receiver<Snapshot>>,
    /// 導入 / 解除の結果。押した直後に画面へ出す。
    action: Option<String>,
    action_rx: Option<Receiver<Result<String, String>>>,
    last_scan: Option<Instant>,
    last_cost: Option<Duration>,
}

fn state() -> &'static Mutex<PanelState> {
    static S: OnceLock<Mutex<PanelState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(PanelState::default()))
}

/// パレットの項目から呼ぶ入口 (開閉を切り替える)。
pub fn toggle_panel() {
    let opened = state().lock().map(|s| s.open).unwrap_or(false);
    if opened {
        if let Ok(mut st) = state().lock() {
            st.open = false;
        }
        return;
    }
    let root = gui_workspace_root();
    if let Ok(mut st) = state().lock() {
        st.open = true;
        st.root = root;
        st.last_scan = None; // 開いた回は必ず取り直す
    }
}

fn spawn_scan(root: PathBuf) -> Receiver<Snapshot> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(scan(&root));
    });
    rx
}

fn spawn_action(root: PathBuf, install_it: bool) -> Receiver<Result<String, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = if install_it {
            install(&root)
        } else {
            uninstall(&root)
        };
        let _ = tx.send(r);
    });
    rx
}

/// 非同期の結果を拾い、必要なら次の走査を出す。**待たない。**
fn poll(st: &mut PanelState, ctx: &egui::Context) {
    if let Some(rx) = &st.action_rx {
        match rx.try_recv() {
            Ok(r) => {
                st.action = Some(match r {
                    Ok(m) => m,
                    Err(e) => e,
                });
                st.action_rx = None;
                st.last_scan = None; // 変えたので取り直す
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => st.action_rx = None,
        }
    }
    if let Some(rx) = &st.pending {
        match rx.try_recv() {
            Ok(s) => {
                st.last_cost = Some(s.cost);
                st.snap = s;
                st.last_scan = Some(Instant::now());
                st.pending = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => st.pending = None,
        }
    }
    if st.pending.is_none() {
        let due = st
            .last_scan
            .is_none_or(|t| t.elapsed() >= crate::git::scan_interval(SCAN_BASE, st.last_cost));
        if due {
            st.pending = Some(spawn_scan(st.root.clone()));
        }
    }
    // 開いている間だけ、結果を拾うために軽く回す。
    ctx.request_repaint_after(Duration::from_millis(400));
}

/// 押されたボタン。
enum Action {
    None,
    Install,
    Uninstall,
    Refresh,
}

/// 毎フレーム呼ばれる描画。**閉じているフレームは 1 ピクセルも触らない**
/// (設計原則 3: アイドル時のコストはゼロ)。
pub fn draw(app: &mut crate::app::ZaivernApp, ctx: &egui::Context) {
    let _ = app; // 状態はモジュール側に持つので app の中身へは触らない
    let Ok(mut st) = state().lock() else { return };
    if !st.open {
        return;
    }
    poll(&mut st, ctx);
    let mut open = true;
    let mut act = Action::None;
    egui::Window::new(tr("🧬 追記の自動マージ — 一覧への追記を衝突させない"))
        .collapsible(false)
        .resizable(true)
        .default_width(640.0)
        .default_height(420.0)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            act = body(ui, &st);
        });
    if !open {
        st.open = false;
    }
    match act {
        Action::None => {}
        Action::Refresh => st.last_scan = None,
        Action::Install | Action::Uninstall => {
            if st.action_rx.is_none() {
                st.action = None;
                st.action_rx = Some(spawn_action(
                    st.root.clone(),
                    matches!(act, Action::Install),
                ));
            }
        }
    }
}

fn body(ui: &mut egui::Ui, st: &PanelState) -> Action {
    let mut act = Action::None;
    let dim = ui.visuals().weak_text_color();
    let busy = st.action_rx.is_some();

    ui.horizontal_wrapped(|ui| {
        let mark = if st.snap.installed {
            tr("導入済み")
        } else {
            tr("未導入")
        };
        ui.label(egui::RichText::new(mark).strong());
        ui.label(
            egui::RichText::new(trf(
                "対象 {n} ファイル",
                &[("n", st.snap.files.len().to_string())],
            ))
            .color(dim),
        );
        if st.snap.truncated > 0 {
            ui.label(
                egui::RichText::new(trf(
                    "(他 {n} 件は上限で打ち切り)",
                    &[("n", st.snap.truncated.to_string())],
                ))
                .color(dim),
            );
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.add_enabled_ui(!busy, |ui| {
            if ui.button(tr("このリポジトリに導入")).clicked() {
                act = Action::Install;
            }
            if ui.button(tr("解除")).clicked() {
                act = Action::Uninstall;
            }
            if ui.button(tr("再読み込み")).clicked() {
                act = Action::Refresh;
            }
        });
        if busy {
            ui.label(egui::RichText::new(tr("実行中…")).color(dim));
        }
    });
    if let Some(m) = &st.action {
        ui.label(egui::RichText::new(m).color(dim));
    }
    if let Some(n) = &st.snap.note {
        ui.label(egui::RichText::new(n).color(ui.visuals().warn_fg_color));
    }
    ui.separator();

    if st.snap.files.is_empty() {
        empty_state(ui, st);
        return act;
    }

    let lay = row_layout(ui.available_width());
    egui::ScrollArea::vertical()
        .id_salt("union.targets")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for f in &st.snap.files {
                let h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
                let (row, _) =
                    ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
                let cells = row_rects(row, &lay);
                let mut cell = cells.iter();
                if let Some(r) = cell.next() {
                    ui.put(*r, egui::Label::new(&f.path).truncate())
                        .on_hover_text(&f.path);
                }
                if lay.show_driver {
                    if let Some(r) = cell.next() {
                        ui.put(
                            *r,
                            egui::Label::new(egui::RichText::new(&f.driver).color(dim)).truncate(),
                        );
                    }
                }
                if let Some(r) = cell.next() {
                    let txt = if f.regions > 0 {
                        trf("領域 {n}", &[("n", f.regions.to_string())])
                    } else {
                        tr("—")
                    };
                    ui.put(*r, egui::Label::new(egui::RichText::new(txt).color(dim)));
                }
            }
        });
    act
}

fn empty_state(ui: &mut egui::Ui, st: &PanelState) {
    let card = empty_card(ui.available_rect_before_wrap());
    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(card), |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(8.0);
            if st.snap.installed {
                ui.label(tr("対象になっているファイルはまだありません。"));
                ui.label(
                    egui::RichText::new(tr(
                        "一覧の周りを zaivern:union-begin / zaivern:union-end で囲むと、両側の追記が自動で残ります。",
                    ))
                    .color(ui.visuals().weak_text_color()),
                );
            } else {
                ui.label(tr("このリポジトリにはまだ導入されていません。"));
                ui.label(
                    egui::RichText::new(tr(
                        "「このリポジトリに導入」を押すと、一覧への追記どうしが衝突しなくなります。",
                    ))
                    .color(ui.visuals().weak_text_color()),
                );
            }
        });
    });
}

// ═══════════════════════════════════════════════════════════════════════
// 12. 登録 — 共有ファイルを 1 バイトも触らずに機能が繋がる入口
// ═══════════════════════════════════════════════════════════════════════

/// 打鍵は割り当てていない。`keybinds::BindAction` は固定長配列 + 件数検査を
/// 持つ最も硬い共有面で、機能ブランチ側から増やすと直列マージが必ず衝突する。
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "union",
    entries: &[crate::feature::Entry {
        icon: "🧬",
        label: "追記の自動マージ — 一覧への追記を衝突させない",
        id: "union.open",
    }],
    dispatch: |_app, _ctx, id| match id {
        "union.open" => {
            toggle_panel();
            true
        }
        _ => false,
    },
    // 窓は中央ビューに属さないオーバーレイなので、毎フレームここから描く。
    // **閉じているときは 1 命令も走らない**ので、アイドル時のコストはゼロ。
    draw: Some(draw),
    settings: &[crate::feature::Setting {
        key: "union.patterns",
        label: "追記の自動マージを適用するファイル (空白区切り)",
        help: "導入時に .gitattributes へ書き込むパターンです。マーカが無いファイルは素の git と同じ挙動のままなので、広めに指定して構いません。",
        default: crate::feature::SettingValue::Text(DEFAULT_PATTERNS),
    }],
    binds: &[],
};

// ═══════════════════════════════════════════════════════════════════════
// 13. テスト
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> UnionOpts {
        UnionOpts::default()
    }

    fn wrapped(body: &str) -> String {
        format!("head\n// {BEGIN}\n{body}// {END}\ntail\n")
    }

    fn merged(base: &str, ours: &str, theirs: &str) -> String {
        match resolve(base, ours, theirs, &opts()) {
            Resolution::Merged(s) => s,
            Resolution::Conflict(s) => panic!("解決されるはずが衝突した:\n{s}"),
        }
    }

    fn conflicted(base: &str, ours: &str, theirs: &str) -> String {
        match resolve(base, ours, theirs, &opts()) {
            Resolution::Conflict(s) => s,
            Resolution::Merged(s) => panic!("衝突として残すはずが解決された:\n{s}"),
        }
    }

    // ── 1. 中核: 両側追記 ──

    #[test]
    fn 両側の追記を両方残す() {
        let base = wrapped("a\n");
        let ours = wrapped("a\nb\n");
        let theirs = wrapped("a\nc\n");
        let out = merged(&base, &ours, &theirs);
        assert_eq!(out, wrapped("a\nb\nc\n"), "ours → theirs の順で両方残る");
    }

    #[test]
    fn 領域の別々の場所への追記も両方残る() {
        let base = wrapped("a\nb\nc\n");
        let ours = wrapped("a\nX\nb\nc\n");
        let theirs = wrapped("a\nb\nc\nY\n");
        assert_eq!(merged(&base, &ours, &theirs), wrapped("a\nX\nb\nc\nY\n"));
    }

    #[test]
    fn 追記の順は常に_ours_から_theirs_で決定的() {
        let base = wrapped("a\n");
        let ours = wrapped("a\nzzz\n");
        let theirs = wrapped("a\naaa\n");
        // 辞書順ではなく **ours が先**。何度やっても同じ。
        let out = merged(&base, &ours, &theirs);
        assert_eq!(out, wrapped("a\nzzz\naaa\n"));
        for _ in 0..5 {
            assert_eq!(merged(&base, &ours, &theirs), out);
        }
    }

    #[test]
    fn sorted_を立てたときだけ辞書順に整列する() {
        let base = wrapped("a\n");
        let ours = wrapped("a\nzzz\n");
        let theirs = wrapped("a\naaa\n");
        let o = UnionOpts {
            sorted: true,
            ..UnionOpts::default()
        };
        let Resolution::Merged(out) = resolve(&base, &ours, &theirs, &o) else {
            panic!("解決されるはず");
        };
        assert_eq!(out, wrapped("a\naaa\nzzz\n"));
    }

    // ── 2. 安全側: 変更・削除は解決しない ──

    #[test]
    fn 片側が既存行を変更したら解決しない() {
        let base = wrapped("a\nb\n");
        let ours = wrapped("a\nb-changed\n");
        let theirs = wrapped("a\nb\nc\n");
        let out = conflicted(&base, &ours, &theirs);
        assert!(out.contains("<<<<<<< ours"), "衝突マーカが要る:\n{out}");
        assert!(out.contains(">>>>>>> theirs"));
    }

    #[test]
    fn 片側が既存行を削除したら解決しない() {
        let base = wrapped("a\nb\n");
        let ours = wrapped("a\n");
        let theirs = wrapped("a\nb\nc\n");
        let out = conflicted(&base, &ours, &theirs);
        assert!(out.contains("======="));
    }

    #[test]
    fn 両側が同じ既存行を書き換えても解決しない() {
        let base = wrapped("a\n");
        let ours = wrapped("a1\n");
        let theirs = wrapped("a2\n");
        conflicted(&base, &ours, &theirs);
    }

    #[test]
    fn 片側だけの変更は衝突にしない_素の三方マージ() {
        let base = wrapped("a\nb\n");
        let ours = wrapped("a\nb\n");
        let theirs = wrapped("a\nB\n");
        assert_eq!(merged(&base, &ours, &theirs), wrapped("a\nB\n"));
    }

    // ── 3. 重複 ──

    #[test]
    fn 両側が同じ行を足したら一本にまとめる() {
        let base = wrapped("a\n");
        let ours = wrapped("a\nsame\n");
        let theirs = wrapped("a\nsame\n");
        // 片側だけの変更にすら見えない (両側同一) ので、そのまま 1 本。
        assert_eq!(merged(&base, &ours, &theirs), wrapped("a\nsame\n"));
    }

    #[test]
    fn 同じ隙間へ両側が同じ行を足したら一本にまとめる() {
        let base = wrapped("a\nb\n");
        let ours = wrapped("a\nx\nsame\nb\n");
        let theirs = wrapped("a\nsame\ny\nb\n");
        // ours の並びを保ったまま、theirs 側の重複 (same) だけが消える。
        assert_eq!(merged(&base, &ours, &theirs), wrapped("a\nx\nsame\ny\nb\n"));
    }

    #[test]
    fn 離れた場所への同一行はどちらも残す_素の三方マージと同じ() {
        // 別々のハンクなので衝突しない = union の出番ではない。
        // ここで畳むと「git なら 2 行になる」結果を勝手に変えることになる。
        let base = wrapped("a\nb\n");
        let ours = wrapped("a\nsame\nb\n");
        let theirs = wrapped("a\nb\nsame\n");
        assert_eq!(merged(&base, &ours, &theirs), wrapped("a\nsame\nb\nsame\n"));
    }

    #[test]
    fn 空行は畳まない() {
        let base = wrapped("a\n");
        let ours = wrapped("a\n\nb\n");
        let theirs = wrapped("a\n\nc\n");
        assert_eq!(merged(&base, &ours, &theirs), wrapped("a\n\nb\n\nc\n"));
    }

    // ── 4. マーカ ──

    #[test]
    fn マーカが無ければ何も解決しない() {
        let base = "a\n";
        let ours = "a\nb\n";
        let theirs = "a\nc\n";
        // 素の git と同じ = 追記どうしはぶつかる。
        conflicted(base, ours, theirs);
    }

    #[test]
    fn whole_file_ならマーカ無しでも解決する() {
        let o = UnionOpts {
            whole_file: true,
            ..UnionOpts::default()
        };
        let Resolution::Merged(out) = resolve("a\n", "a\nb\n", "a\nc\n", &o) else {
            panic!("whole_file なら解決するはず");
        };
        assert_eq!(out, "a\nb\nc\n");
    }

    #[test]
    fn 領域の外側の追記は解決しない() {
        let base = format!("head\n// {BEGIN}\nx\n// {END}\n");
        let ours = format!("head\nOURS\n// {BEGIN}\nx\n// {END}\n");
        let theirs = format!("head\nTHEIRS\n// {BEGIN}\nx\n// {END}\n");
        conflicted(&base, &ours, &theirs);
    }

    #[test]
    fn 閉じていないマーカでは一切解決しない() {
        let base = format!("// {BEGIN}\na\n");
        let ours = format!("// {BEGIN}\na\nb\n");
        let theirs = format!("// {BEGIN}\na\nc\n");
        conflicted(&base, &ours, &theirs);
    }

    #[test]
    fn 入れ子のマーカでも一切解決しない() {
        let base = format!("// {BEGIN}\n// {BEGIN}\na\n// {END}\n");
        let ours = format!("// {BEGIN}\n// {BEGIN}\na\nb\n// {END}\n");
        let theirs = format!("// {BEGIN}\n// {BEGIN}\na\nc\n// {END}\n");
        conflicted(&base, &ours, &theirs);
    }

    #[test]
    fn 片側がマーカ行を消したら解決しない() {
        let base = wrapped("a\n");
        let ours = "head\na\nb\ntail\n".to_string();
        let theirs = wrapped("a\nc\n");
        conflicted(&base, &ours, &theirs);
    }

    #[test]
    fn 領域が二つあってもそれぞれ独立に解決する() {
        let base = format!("// {BEGIN}\na\n// {END}\nmid\n// {BEGIN}\np\n// {END}\n");
        let ours = format!("// {BEGIN}\na\nb\n// {END}\nmid\n// {BEGIN}\np\nq\n// {END}\n");
        let theirs = format!("// {BEGIN}\na\nc\n// {END}\nmid\n// {BEGIN}\np\nr\n// {END}\n");
        let out = merged(&base, &ours, &theirs);
        assert_eq!(
            out,
            format!("// {BEGIN}\na\nb\nc\n// {END}\nmid\n// {BEGIN}\np\nq\nr\n// {END}\n")
        );
    }

    // ── 5. 改行コードと末尾改行 ──

    #[test]
    fn crlf_のファイルは_crlf_のまま返る() {
        let base = wrapped("a\n").replace('\n', "\r\n");
        let ours = wrapped("a\nb\n").replace('\n', "\r\n");
        let theirs = wrapped("a\nc\n").replace('\n', "\r\n");
        let out = merged(&base, &ours, &theirs);
        assert_eq!(out, wrapped("a\nb\nc\n").replace('\n', "\r\n"));
        assert!(!out.contains("\n\r"), "改行が壊れている");
    }

    #[test]
    fn 混在した改行は行ごとに元のものを保つ() {
        let base = format!("// {BEGIN}\r\na\n// {END}\r\n");
        let ours = format!("// {BEGIN}\r\na\nb\r\n// {END}\r\n");
        let theirs = format!("// {BEGIN}\r\na\nc\n// {END}\r\n");
        let out = merged(&base, &ours, &theirs);
        assert_eq!(out, format!("// {BEGIN}\r\na\nb\r\nc\n// {END}\r\n"));
    }

    #[test]
    fn 末尾に改行が無ければ無いまま返る() {
        let base = format!("// {BEGIN}\na\n// {END}");
        let ours = format!("// {BEGIN}\na\nb\n// {END}");
        let theirs = format!("// {BEGIN}\na\nc\n// {END}");
        let out = merged(&base, &ours, &theirs);
        assert_eq!(out, format!("// {BEGIN}\na\nb\nc\n// {END}"));
        assert!(!out.ends_with('\n'));
    }

    #[test]
    fn 末尾に改行があれば残る() {
        let base = wrapped("a\n");
        assert!(merged(&base, &wrapped("a\nb\n"), &wrapped("a\nc\n")).ends_with('\n'));
    }

    #[test]
    fn 改行の無い最終行の後ろへ追記しても行が繋がらない() {
        let base = format!("// {BEGIN}\n// {END}\nlast");
        let ours = format!("// {BEGIN}\nb\n// {END}\nlast");
        let theirs = format!("// {BEGIN}\nc\n// {END}\nlast");
        let out = merged(&base, &ours, &theirs);
        assert_eq!(out, format!("// {BEGIN}\nb\nc\n// {END}\nlast"));
    }

    // ── 6. 境界 ──

    #[test]
    fn 空のファイルでも落ちない() {
        assert_eq!(merged("", "", ""), "");
        assert!(matches!(
            resolve("", "a\n", "b\n", &opts()),
            Resolution::Conflict(_)
        ));
    }

    #[test]
    fn 三者が同一なら何も変わらない() {
        let s = wrapped("a\nb\n");
        assert_eq!(merged(&s, &s, &s), s);
    }

    #[test]
    fn 大きな領域でも追記なら解決する() {
        let body: String = (0..2000).map(|i| format!("line{i}\n")).collect();
        let base = wrapped(&body);
        let ours = wrapped(&format!("{body}ours\n"));
        let theirs = wrapped(&format!("{body}theirs\n"));
        assert_eq!(
            merged(&base, &ours, &theirs),
            wrapped(&format!("{body}ours\ntheirs\n"))
        );
    }

    #[test]
    fn マーカ長は_l_の指定に従う() {
        let o = UnionOpts {
            marker_size: 11,
            ..UnionOpts::default()
        };
        let Resolution::Conflict(out) = resolve("a\n", "b\n", "c\n", &o) else {
            panic!("衝突するはず");
        };
        assert!(out.contains("<<<<<<<<<<< ours"), "{out}");
        assert!(out.contains("\n===========\n"), "{out}");
    }

    // ── 7. 差分の下請け ──

    #[test]
    fn 差分の対応は両側について単調増加() {
        let a = ["a", "b", "c", "d", "e"];
        let b = ["a", "x", "c", "d", "y", "e"];
        let p = diff_pairs(&a, &b);
        assert!(p.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1));
        assert_eq!(p, vec![(0, 0), (2, 2), (3, 3), (4, 5)]);
    }

    #[test]
    fn 一意行アンカーは順序が保たれる組だけを返す() {
        // b 側で順序が入れ替わっている組は片方しか採らない。
        let a = ["p", "q"];
        let b = ["q", "p"];
        let anchors = unique_anchors(&a, &b);
        assert_eq!(anchors.len(), 1);
    }

    #[test]
    fn 追記だけなら足場が全部見つかる() {
        let b = split_lines("a\nb\n");
        let s = split_lines("a\nx\nb\ny\n");
        let (ins, anchor) = align(&b, &s).expect("追記だけなので通る");
        assert_eq!(anchor.len(), 2);
        assert_eq!(ins[1].len(), 1);
        assert_eq!(ins[2].len(), 1);
    }

    #[test]
    fn 並べ替えは追記と見なさない() {
        let b = split_lines("a\nb\n");
        assert!(align(&b, &split_lines("b\na\n")).is_none());
    }

    // ── 8. 行の分解 ──

    #[test]
    fn 行の分解と再結合は元へ戻る() {
        for s in [
            "",
            "a",
            "a\n",
            "a\r\nb\n",
            "\n\n",
            "a\nb",
            "\r\n",
            "no newline at eof",
        ] {
            let lines = split_lines(s);
            assert_eq!(join_lines(&lines, "\n"), s, "入力 {s:?}");
        }
    }

    // ── 9. 導入 / 解除 (git を使う) ──

    fn git_available() -> bool {
        crate::procx::hidden_command("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// `git init` 済みの使い捨てリポジトリ。実ホームには一切触れない。
    fn temp_repo(tag: &str) -> Option<PathBuf> {
        if !git_available() {
            return None;
        }
        let dir = crate::test_util::unique_temp_dir("zv-union", tag);
        let run = |args: &[&str]| {
            crate::procx::hidden_command("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !run(&["init", "-q", "-b", "main"]) {
            return None;
        }
        run(&["config", "user.email", "t@example.invalid"]);
        run(&["config", "user.name", "zaivern test"]);
        run(&["config", "commit.gpgsign", "false"]);
        Some(dir)
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let out = crate::procx::hidden_command("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "zaivern test")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "zaivern test")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .expect("git を起動できない");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn 導入と解除は何度やっても同じ結果になる() {
        let Some(repo) = temp_repo("install") else {
            println!("git が無い環境なのでスキップ");
            return;
        };
        let exe = repo.join("fake-zai");
        let pats = vec!["*.md".to_string()];
        for _ in 0..2 {
            install_with(&repo, &exe, &pats).expect("導入");
        }
        assert!(is_installed(&repo));
        let attrs = std::fs::read_to_string(repo.join(".gitattributes")).expect("読める");
        assert_eq!(
            attrs.matches(ATTR_BEGIN_KEY).count(),
            1,
            "管理ブロックは 1 つだけ:\n{attrs}"
        );
        assert!(attrs.contains("*.md merge=zaivern-union"));
        for _ in 0..2 {
            uninstall(&repo).expect("解除");
        }
        assert!(!is_installed(&repo));
        assert!(
            !repo.join(".gitattributes").exists(),
            "こちらが作ったファイルは消す"
        );
    }

    #[test]
    fn 既存の_gitattributes_の行を壊さない() {
        let Some(repo) = temp_repo("attrs") else {
            println!("git が無い環境なのでスキップ");
            return;
        };
        let keep = "*.png binary\n*.txt text eol=lf\n";
        std::fs::write(repo.join(".gitattributes"), keep).expect("write");
        install_with(&repo, &repo.join("fake-zai"), &["*.md".to_string()]).expect("導入");
        let after = std::fs::read_to_string(repo.join(".gitattributes")).expect("読める");
        assert!(after.starts_with(keep), "既存の行が先頭に残る:\n{after}");
        uninstall(&repo).expect("解除");
        let back = std::fs::read_to_string(repo.join(".gitattributes")).expect("読める");
        assert_eq!(back, keep, "解除したら元どおり");
    }

    #[test]
    fn 終了行を消されたブロックでも後ろの行を巻き込まない() {
        let broken = format!(
            "*.png binary\n{ATTR_BEGIN}\n*.md merge=zaivern-union\n# 人が後から足した行\n*.txt text\n"
        );
        let (kept, _) = strip_block(&broken);
        assert_eq!(kept, "*.png binary\n# 人が後から足した行\n*.txt text\n");
    }

    #[test]
    fn 管理ブロックの中身の見分け() {
        assert!(is_generated_attr_line("*.md merge=zaivern-union"));
        assert!(is_generated_attr_line("CHANGELOG.md merge=zaivern-union-whole"));
        assert!(is_generated_attr_line("   "));
        assert!(!is_generated_attr_line("*.png binary"));
        assert!(!is_generated_attr_line("*.md merge=zaivern-union text"));
        assert!(!is_generated_attr_line("# コメント"));
    }

    #[test]
    fn ドライバのコマンドは空白入りのパスでも壊れない() {
        let p = PathBuf::from("/a b/zai's tool/zai");
        let cmd = driver_command(&p, "--whole");
        assert!(cmd.starts_with('\''), "引用されている: {cmd}");
        assert!(cmd.ends_with("merge-driver --whole %O %A %B %L %P"));
        assert!(cmd.contains(r"zai'\''s"), "単引用符が閉じられている: {cmd}");
    }

    #[test]
    fn パターンの指定は空白でもカンマでも同じに割れる() {
        assert_eq!(split_patterns("*.md, *.toml"), vec!["*.md", "*.toml"]);
        assert_eq!(split_patterns("*.md\t*.toml"), vec!["*.md", "*.toml"]);
        // 空なら既定へ戻す (空のブロックを書かない)
        assert_eq!(
            split_patterns("   "),
            DEFAULT_PATTERNS.split(' ').collect::<Vec<_>>()
        );
    }

    // ── 10. 統合: 本物の git に本当にマージさせる ──

    /// `git merge` から呼ばれるドライバの実体。**通常のテスト実行では即座に
    /// 何もせず終わる** (環境変数が無いため)。
    ///
    /// テストバイナリには `zai` の CLI 入口が無いので、統合テストは自分自身を
    /// ドライバとして登録する。git はドライバをシェル経由で起動するので、
    /// 引数は環境変数で受け渡す (libtest は位置引数をテスト名の絞り込みと
    /// 解釈してしまうため)。
    #[test]
    fn merge_driver_helper() {
        let Ok(o) = std::env::var("ZV_UNION_O") else {
            return;
        };
        let argv: Vec<String> = ["ZV_UNION_A", "ZV_UNION_B", "ZV_UNION_L", "ZV_UNION_P"]
            .iter()
            .map(|k| std::env::var(k).unwrap_or_default())
            .collect();
        let mut all = vec![o];
        all.extend(argv);
        std::process::exit(cli_main(&all));
    }

    /// このヘルパの libtest 上の名前 (`--exact` に渡す)。
    /// `module_path!()` から起こすので、モジュールを動かしてもずれない。
    fn helper_test_name() -> String {
        let m = module_path!();
        let rel = m.split_once("::").map(|(_, r)| r).unwrap_or(m);
        format!("{rel}::merge_driver_helper")
    }

    #[test]
    fn 本物の_git_マージで追記どうしが自動解決される() {
        let Some(repo) = temp_repo("merge") else {
            println!("git が無い環境なのでスキップ");
            return;
        };
        let exe = std::env::current_exe().expect("テストバイナリの場所");
        // git はドライバを `sh -c` で起動する。引数は環境変数で渡す。
        let driver = format!(
            "ZV_UNION_O=\"%O\" ZV_UNION_A=\"%A\" ZV_UNION_B=\"%B\" ZV_UNION_L=\"%L\" ZV_UNION_P=\"%P\" {} --exact {} --quiet",
            sh_quote(&exe),
            helper_test_name()
        );
        git(&repo, &["config", "--local", "merge.zaivern-union.name", DRIVER_DESC]);
        git(&repo, &["config", "--local", "merge.zaivern-union.driver", &driver]);
        std::fs::write(repo.join(".gitattributes"), "list.md merge=zaivern-union\n").expect("write");

        let base = wrapped("- a\n");
        std::fs::write(repo.join("list.md"), &base).expect("write");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "base"]);

        // 枝 A: 末尾へ 1 行足す
        git(&repo, &["checkout", "-q", "-b", "featA"]);
        std::fs::write(repo.join("list.md"), wrapped("- a\n- from A\n")).expect("write");
        git(&repo, &["commit", "-qam", "A"]);

        // 枝 B: **同じ場所へ**別の 1 行を足す (素の git なら必ず衝突する)
        git(&repo, &["checkout", "-q", "main"]);
        git(&repo, &["checkout", "-q", "-b", "featB"]);
        std::fs::write(repo.join("list.md"), wrapped("- a\n- from B\n")).expect("write");
        git(&repo, &["commit", "-qam", "B"]);

        let out = git(&repo, &["merge", "--no-edit", "featA"]);
        let merged_text = std::fs::read_to_string(repo.join("list.md")).expect("読める");
        assert!(
            !merged_text.contains("<<<<<<<"),
            "ドライバが効いていない (衝突マーカが残った)。git の出力:\n{out}\n中身:\n{merged_text}"
        );
        // ours = featB (マージ先) が先、theirs = featA が後。
        assert_eq!(merged_text, wrapped("- a\n- from B\n- from A\n"));

        let status = git(&repo, &["status", "--porcelain"]);
        assert!(
            status.trim().is_empty(),
            "マージが完了していない: {status:?}"
        );
    }

    #[test]
    fn 本物の_git_マージでも変更どうしはちゃんと衝突する() {
        let Some(repo) = temp_repo("merge-conflict") else {
            println!("git が無い環境なのでスキップ");
            return;
        };
        let exe = std::env::current_exe().expect("テストバイナリの場所");
        let driver = format!(
            "ZV_UNION_O=\"%O\" ZV_UNION_A=\"%A\" ZV_UNION_B=\"%B\" ZV_UNION_L=\"%L\" ZV_UNION_P=\"%P\" {} --exact {} --quiet",
            sh_quote(&exe),
            helper_test_name()
        );
        git(&repo, &["config", "--local", "merge.zaivern-union.name", DRIVER_DESC]);
        git(&repo, &["config", "--local", "merge.zaivern-union.driver", &driver]);
        std::fs::write(repo.join(".gitattributes"), "list.md merge=zaivern-union\n").expect("write");
        std::fs::write(repo.join("list.md"), wrapped("- a\n")).expect("write");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "base"]);

        git(&repo, &["checkout", "-q", "-b", "featA"]);
        std::fs::write(repo.join("list.md"), wrapped("- a from A\n")).expect("write");
        git(&repo, &["commit", "-qam", "A"]);
        git(&repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join("list.md"), wrapped("- a from B\n")).expect("write");
        git(&repo, &["commit", "-qam", "B"]);

        git(&repo, &["merge", "--no-edit", "featA"]);
        let text = std::fs::read_to_string(repo.join("list.md")).expect("読める");
        assert!(
            text.contains("<<<<<<<") && text.contains(">>>>>>>"),
            "既存行の書き換えどうしは人間に返すこと:\n{text}"
        );
    }

    #[test]
    fn マーカの無いファイルは素の_git_と同じ結果になる() {
        let Some(repo) = temp_repo("plain") else {
            println!("git が無い環境なのでスキップ");
            return;
        };
        let exe = std::env::current_exe().expect("テストバイナリの場所");
        let driver = format!(
            "ZV_UNION_O=\"%O\" ZV_UNION_A=\"%A\" ZV_UNION_B=\"%B\" ZV_UNION_L=\"%L\" ZV_UNION_P=\"%P\" {} --exact {} --quiet",
            sh_quote(&exe),
            helper_test_name()
        );
        git(&repo, &["config", "--local", "merge.zaivern-union.name", DRIVER_DESC]);
        git(&repo, &["config", "--local", "merge.zaivern-union.driver", &driver]);
        std::fs::write(repo.join(".gitattributes"), "plain.md merge=zaivern-union\n").expect("write");
        // 離れた場所への変更は git が普通に取り込む (ドライバがあっても同じ)。
        let body: String = (1..=20).map(|i| format!("l{i}\n")).collect();
        std::fs::write(repo.join("plain.md"), &body).expect("write");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "base"]);

        git(&repo, &["checkout", "-q", "-b", "featA"]);
        std::fs::write(repo.join("plain.md"), body.replace("l2\n", "l2-A\n")).expect("write");
        git(&repo, &["commit", "-qam", "A"]);
        git(&repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join("plain.md"), body.replace("l19\n", "l19-B\n")).expect("write");
        git(&repo, &["commit", "-qam", "B"]);

        git(&repo, &["merge", "--no-edit", "featA"]);
        let text = std::fs::read_to_string(repo.join("plain.md")).expect("読める");
        assert!(!text.contains("<<<<<<<"), "素の git なら通るはず:\n{text}");
        assert!(text.contains("l2-A") && text.contains("l19-B"));
    }

    // ── 11. レイアウト (純粋関数) ──

    #[test]
    fn 空状態カードは可用領域の中央に必ず収まる() {
        for (w, h) in [(900.0, 700.0), (1200.0, 300.0), (320.0, 200.0), (40.0, 40.0)] {
            let avail = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(w, h));
            let card = empty_card(avail);
            assert!(avail.contains_rect(card), "{w}x{h} で外へはみ出した");
            assert!(
                (card.center() - avail.center()).length() < 0.01,
                "{w}x{h} で中央に無い"
            );
        }
    }

    #[test]
    fn 行のセルはどの幅でも可用領域に収まり重ならない() {
        for w in [1200.0_f32, 900.0, 640.0, 420.0, 380.0, 200.0, 60.0, 0.0] {
            let lay = row_layout(w);
            let row = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, 20.0));
            let cells = row_rects(row, &lay);
            for c in &cells {
                assert!(row.contains_rect(*c), "幅 {w} で列がはみ出した: {c:?}");
            }
            for pair in cells.windows(2) {
                assert!(
                    pair[0].right() <= pair[1].left() + 0.01,
                    "幅 {w} で列が重なった: {pair:?}"
                );
            }
        }
    }

    #[test]
    fn 狭い幅ではドライバ名の列を畳む() {
        assert!(row_layout(640.0).show_driver);
        assert!(!row_layout(300.0).show_driver);
        assert_eq!(row_layout(300.0).driver_w, 0.0);
        assert_eq!(row_rects(
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(300.0, 20.0)),
            &row_layout(300.0)
        ).len(), 2);
    }

    // ── 12. 登録 ──

    #[test]
    fn 登録の_id_はモジュール接頭辞で設定キーも同じ接頭辞になっている() {
        assert_eq!(FEATURE.module, "union");
        for e in FEATURE.entries {
            assert!(e.id.starts_with("union."), "{:?}", e.id);
        }
        for s in FEATURE.settings {
            assert!(s.key.starts_with("union."), "{:?}", s.key);
        }
        assert!(FEATURE.draw.is_some(), "パネルを描く経路が要る");
        assert!(FEATURE.binds.is_empty(), "打鍵は統合担当が入れる");
    }

    /// **UI スレッドで git を待たない**という約束の番人 (`git.rs` と同じ趣旨)。
    #[test]
    fn 描画から同期gitを撃つ経路が残っていない() {
        let src = include_str!("union.rs").replace("\r\n", "\n");
        for (name, sig) in [
            ("draw", "pub fn draw(app: &mut crate::app::ZaivernApp, ctx: &egui::Context) {"),
            ("body", "fn body(ui: &mut egui::Ui, st: &PanelState) -> Action {"),
        ] {
            let body = src
                .split(sig)
                .nth(1)
                .unwrap_or_else(|| panic!("{name} が見つからない"));
            let body = body.split("\n}\n").next().expect("本体の終端");
            for needle in ["git_out(", "scan(", "install(", "uninstall("] {
                assert!(
                    !body.contains(needle),
                    "{name} が {needle} を同期で撃っている (フレームが git を待つ)"
                );
            }
        }
        // 走査と導入は必ず裏のスレッドから呼ぶ。
        assert!(src.contains("std::thread::spawn"), "裏のスレッドが無い");
    }

    /// 共有ファイルへ 1 行も足していないこと (この機能の存在意義そのもの)。
    #[test]
    fn 共有ファイルを触らずに繋がっている() {
        let reg = include_str!("features/union.rs").replace("\r\n", "\n");
        assert!(reg.contains("#[path = \"../union.rs\"]"), "実体の引き込みが無い");
        assert!(reg.contains("pub use imp::{cli_main, FEATURE};"), "再エクスポートが無い");
    }
}
