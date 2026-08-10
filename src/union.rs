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
//! * `zaivern:union-begin` / `zaivern:union-end` を含む行で囲まれた
//!   **領域の内側だけ**を対象にする。コメント記号を見ないので
//!   Rust / JS / Python / TOML / YAML / Markdown どれでも同じ書き方で効く。
//! * マーカが 1 つも無いファイルは、自動判定 (`--auto`) が
//!   **「これは 1 行 1 要素の一覧だ」と確信できたときだけ**対象になる。
//!   確信できなければ [`cli_main`] は `git merge-file` へそのまま委譲するので、
//!   **素の git と 1 バイトも変わらない**結果になる。
//! * 順序は決定的 (ours → theirs)。`HashMap` / `HashSet` を使わず
//!   `Vec` / `BTreeSet` / `BTreeMap` だけで組んであるので、反復順が出力へ漏れない。
//!
//! ## マーカ無しで効かせる (`--auto`) — どのリポジトリでも衝突を消すために
//!
//! マーカ方式は **zaivern 自身のリポジトリでしか効かない**。ユーザーの要求は
//! 「どのリポジトリでも衝突が起きない」なので、`zaivern-union-auto` ドライバは
//! **中身を見て**一覧を探す。拡張子やファイル名は
//! [`suggest_attributes`] がパターンを選ぶときにしか使わない
//! (対象にしても、中身が一覧でなければ何も起きない)。
//!
//! | 種類 ([`ListKind`]) | 見つけ方 (中身だけ) | 重複行 | 構文検査 |
//! |---|---|---|---|
//! | `Flat` | 全行が同じ字下げの「1 行 1 要素」。`.gitignore` / `requirements.txt` / `go.sum` / `.env` | **畳む** (一覧は集合) | 重複キー |
//! | `Imports` | `use` / `mod` / `import` / `export` / `#include` が 3 行以上続くブロック | **畳む** | 重複キー |
//! | `Journal` | 見出し + 箇条書きが本体。`CHANGELOG.md` / `NEWS` | **畳まない** (同じ文面の 2 件がありうる) | 見出しの重複 |
//! | `Bracket` | `{` / `[` で開いて閉じるまでが全部 1 行 1 要素。JSON / 配列リテラル | **畳む** | JSON パーサ / 括弧の釣り合い |
//! | `TomlSection` | `[section]` + `key = value` だけのファイル | **畳む** | 重複キー (セクション毎) |
//!
//! ### 自動判定でも「誤って解決しない」ための 4 段
//!
//! 1. **3 版すべて**が同じ種類・同じブロック数に見えなければ、その回は降りる。
//! 2. 片側でも既存行を変更・削除していたら、その領域は解決しない (マーカ方式と同じ)。
//! 3. 両側が**同じキー**の行を足していて中身が違えば解決しない
//!    (`"serde": "1"` と `"serde": "2"`、`## 0.2.0` と `## 0.2.0`)。
//! 4. 出来上がりを**構文検査**に通す。落ちたら結果を捨てる。
//!
//! そして自動判定モードでは **「全部解決できたときだけ」結果を差し替える**。
//! 1 つでも衝突が残ったら `git merge-file` へ委譲するので、
//! **自分の衝突マーカを一度も書かない** = 「入れたら見た目が変わった」が起きない。
//!
//! ### 実測 (`tools/union-bench.sh`、マーカを 1 つも置かない合成リポジトリ)
//!
//! `.gitignore` / `package.json` / `CHANGELOG.md` / `mod` ブロック /
//! **一覧ではない** `code.rs` の 5 つへ N 人が同時に追記したときの総衝突行:
//!
//! | 人数 | 素の git | `--auto` | 削減 | 誤自動解決 |
//! |---:|---:|---:|---:|---:|
//! | 4 | 45 | 9 | 80% | 0 |
//! | 8 | 175 | 35 | 80% | 0 |
//! | 16 | 675 | 135 | 80% | 0 |
//!
//! **残る 20% は `code.rs` (一覧ではないもの) で、そこは効かないのが正しい。**
//! `--auto` を付けない条件はベースラインと**完全に同じ数字**になる。
//!
//! ### 提案 ([`suggest_attributes`]) は「ファイル全体が一覧」のものだけ
//!
//! `Imports` とコードの配列リテラルは**ブロック単位**の判定なので、3 行を
//! 根拠に `*.rs` のような広いパターンを書くことになる (実測: このリポジトリの
//! `.rs` 124 個のうち 102 個が該当し、`*.rs` が提案された)。効果に対して
//! 影響範囲が大きすぎるので提案からは外してある — **明示的に
//! `.gitattributes` へ書けば今までどおり効く**。この絞り込みを入れた後、
//! このリポジトリでの提案は **8 パターン** (`.gitignore` / `CHANGELOG.md` /
//! `Cargo.toml` / `plugin.toml` ×10 など) になった。
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
//! | `zaivern-union` | マーカの内側だけ (**明示指定は自動判定より強い**) |
//! | `zaivern-union-auto` | マーカがあればその内側、無ければ**中身から一覧を探す** |
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
    (AUTO_DRIVER, "--auto"),
    ("zaivern-union-whole", "--whole"),
    ("zaivern-union-sorted", "--whole --sorted"),
];

/// `.gitattributes` の提案が既定で当てるドライバ。**マーカ無しで効く唯一のもの。**
pub const AUTO_DRIVER: &str = "zaivern-union-auto";

/// `git config merge.<名前>.name` に書く説明。
const DRIVER_DESC: &str = "Zaivern: 追記どうしの衝突だけを自動で解決する";

/// 設定 `union.patterns` を**空にしなかった**ときのための保険。
/// 既定は空 = [`suggest_attributes`] にリポジトリを見て決めさせる。
const DEFAULT_PATTERNS: &str = "*.md *.toml *.txt *.json *.yaml *.yml";

/// [`suggest_attributes`] が中身を読むファイル数の上限。
const MAX_SUGGEST_READ: usize = 2000;
/// 同上、1 ファイルのバイト数の上限 (大きいものは一覧ではない)。
const MAX_SUGGEST_BYTES: u64 = 256 * 1024;
/// まとめられなかったファイルを個別に並べる上限 (`.gitattributes` を汚さない)。
const MAX_SUGGEST_PATHS: usize = 20;

/// 拡張子でまとめず**その名前のまま**パターンにするファイル。
/// あくまで「パターンの書き方」の話で、**対象にするかは中身が決める**。
const WELL_KNOWN: &[&str] = &[
    ".gitignore",
    ".dockerignore",
    ".npmignore",
    ".eslintignore",
    ".prettierignore",
    ".env",
    ".env.example",
    "CHANGELOG.md",
    "CHANGELOG",
    "NEWS",
    "NEWS.md",
    "HISTORY.md",
    "CODEOWNERS",
    "Gemfile",
    "Dockerfile",
    "go.sum",
    "go.mod",
    "requirements.txt",
    "requirements-dev.txt",
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
];

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
//  4.5 自動判定 — マーカが 1 つも無いリポジトリで効かせる
// ═══════════════════════════════════════════════════════════════════════
//
// ここが本丸。マーカ方式は「囲んだ人がいる」ことが前提なので、**他人の
// リポジトリでは 1 度も効かない**。どのリポジトリにも一覧はあるので、
// 中身から見つけて同じ安全側の規則を当てる。
//
// **確信できなければ何もしない**のが唯一の設計方針である。判定は
// 「これは一覧だ」を*積極的に*証明できたときだけ真を返す (拡張子で
// 決め打ちしない)。証明できなければ [`cli_main`] が `git merge-file` へ
// 委譲するので、素の git と 1 バイトも変わらない。

/// 自動判定で見つけた「一覧」の種類。表と根拠はモジュール冒頭のドキュメント。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ListKind {
    /// 1 行 1 要素の平坦な一覧。`.gitignore` / `requirements.txt` / `go.sum` /
    /// `.env` / `mod` 宣言だけのファイルなど。
    Flat,
    /// 宣言 (`use` / `mod` / `import` / `export` / `#include`) が 3 行以上
    /// 続くブロック。**ファイルの他の部分は対象にしない。**
    Imports,
    /// 見出しつきの追記帳。`CHANGELOG.md` / `NEWS`。**重複を畳まない。**
    Journal,
    /// 括弧で囲まれた平坦な本体。JSON のオブジェクト / 配列、
    /// コードの配列リテラル (`&[` / `[`)。
    Bracket,
    /// TOML の `[section]` 本体。
    TomlSection,
}

impl ListKind {
    /// 画面と `.gitattributes` の理由欄に出す短い名前。
    pub fn label(self) -> &'static str {
        match self {
            ListKind::Flat => "1行1要素の一覧",
            ListKind::Imports => "宣言の連続ブロック",
            ListKind::Journal => "見出しつきの追記帳",
            ListKind::Bracket => "括弧で囲んだ一覧",
            ListKind::TomlSection => "[section] の表",
        }
    }

    /// 重複行の扱い。**種類ごとに決めてある** (モジュール冒頭の表が仕様)。
    ///
    /// 一覧は集合なので同じ行が 2 本あるのは誤り。CHANGELOG は「同じ文面の
    /// 2 件」がありうる (別々の人が同じ修正を書いた) ので畳まない。
    fn dedup(self) -> Dedup {
        match self {
            ListKind::Journal => Dedup::Keep,
            _ => Dedup::Fold,
        }
    }
}

/// 両側が同じ行を足したときの扱い。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dedup {
    /// 1 本に畳む。
    Fold,
    /// 2 本とも残す。
    Keep,
}

/// 結果を返す前に通す構文検査。**落ちたら結果ごと捨てる。**
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Check {
    /// 重複キーの検査だけ (全種類共通)。
    Off,
    /// JSON として読み直す (重複キーも弾く)。
    Json,
    /// TOML として読み直す (セクション毎の重複キーも弾く)。
    Toml,
    /// 括弧の釣り合いが base と同じままか。
    Brackets,
}

/// 1 つの版について「どこが一覧の本文か」。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Plan {
    kind: ListKind,
    check: Check,
    /// 一覧の本文 (行の半開区間)。**重ならず、開始位置の昇順。**
    blocks: Vec<(usize, usize)>,
}

/// 1 行 1 要素と認めるときの上限 (これを超えたら散文とみなす)。
const MAX_ITEM_WORDS: usize = 8;
/// 同上、1 行の文字数。
const MAX_ITEM_CHARS: usize = 200;
/// 一覧と認めるのに要る最小の要素数。
const MIN_ITEMS: usize = 3;

/// 中身から一覧の種類を見つける。**分からなければ `None`。**
///
/// 画面表示と [`suggest_attributes`] からも使う (1 版だけ見るとき)。
pub fn detect(text: &str) -> Option<ListKind> {
    detect_lines(&split_lines(text)).map(|p| p.kind)
}

/// 3 版すべてが同じ形の一覧に見えるか。[`cli_main`] の「委譲するか」の判断。
pub fn auto_applies(base: &str, ours: &str, theirs: &str) -> bool {
    plans_for(
        &split_lines(base),
        &split_lines(ours),
        &split_lines(theirs),
    )
    .is_some()
}

/// 3 版ぶんの計画。**種類もブロック数も一致したときだけ** `Some`。
///
/// 片側だけが一覧をやめた / セクションを増やしたなら、その回は降りる。
fn plans_for(b: &[Line], o: &[Line], t: &[Line]) -> Option<(Plan, Plan, Plan)> {
    let pb = detect_lines(b)?;
    let po = detect_lines(o)?;
    let pt = detect_lines(t)?;
    if pb.kind != po.kind || po.kind != pt.kind {
        return None;
    }
    if pb.check != po.check || po.check != pt.check {
        return None;
    }
    if pb.blocks.len() != po.blocks.len() || po.blocks.len() != pt.blocks.len() {
        return None;
    }
    if pb.blocks.is_empty() {
        return None;
    }
    Some((pb, po, pt))
}

/// 判定の本体。**狭い順に試す** (TOML / JSON は平坦な行の集合にも見えるので先)。
fn detect_lines(lines: &[Line]) -> Option<Plan> {
    if lines.is_empty() {
        return None;
    }
    let t: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
    detect_journal(&t)
        .or_else(|| detect_toml(&t))
        .or_else(|| detect_json(&t))
        .or_else(|| detect_flat(&t))
        .or_else(|| detect_bracket(&t))
        .or_else(|| detect_imports(&t))
}

// ── 行を読むための小道具 ────────────────────────────────────────

/// 先頭の空白。
fn indent_of(s: &str) -> &str {
    &s[..s.len() - s.trim_start().len()]
}

/// 行の括弧の増減と、途中の最小値。**二重引用符の中は読み飛ばす。**
///
/// 文字列を飛ばすのは `"a(b": 1` のような JSON のキーで釣り合いを
/// 誤判定しないため。引用符が閉じない行は「読めない」として
/// `(0, -1)` を返し、どの判定にも通さない。
fn line_delta(s: &str) -> (i32, i32) {
    let mut d = 0i32;
    let mut lo = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for c in s.chars() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '(' | '[' | '{' => d += 1,
            ')' | ']' | '}' => {
                d -= 1;
                lo = lo.min(d);
            }
            _ => {}
        }
    }
    if in_str {
        return (0, -1);
    }
    (d, lo)
}

/// その行だけで括弧が閉じているか。
fn balanced(s: &str) -> bool {
    let (d, lo) = line_delta(s);
    d == 0 && lo >= 0
}

/// 文末記号で終わる行は**散文**とみなす。一覧の要素は文で終わらない。
fn sentence_like(t: &str) -> bool {
    matches!(
        t.chars().last(),
        Some('.' | '。' | '!' | '！' | '?' | '？' | '、' | '，')
    )
}

/// 「1 行 1 要素」と認める形か。
///
/// `{}` と `()` を 1 文字でも含んだら降りるのが要 (これが無いと
/// `fn a() {}` が 3 行並んだだけのファイルを「一覧」と読んでしまい、
/// **関数を足しただけの本物の衝突を勝手に解決する**)。`[]` は残してある —
/// `.gitignore` の `[abc]` や `requirements.txt` の `foo[extra]` が要る。
fn flat_item(t: &str) -> bool {
    if t.is_empty() || t.chars().count() > MAX_ITEM_CHARS {
        return false;
    }
    if t.contains(['{', '}', '(', ')']) {
        return false;
    }
    if !balanced(t) || sentence_like(t) {
        return false;
    }
    if t.split_whitespace().count() > MAX_ITEM_WORDS {
        return false;
    }
    // 次の行へ続く形 (継続・ブロックの開始) は 1 要素ではない。
    // `=` は除いてある — go.sum のハッシュは base64 の `=` で終わる。
    !matches!(t.chars().last(), Some(',' | '\\' | ':' | '{' | '[' | '('))
}

// ── 種類ごとの判定 ──────────────────────────────────────────────

/// 見出しつきの追記帳 (`CHANGELOG.md` / `NEWS`)。
///
/// 「見出しが 1 つ以上」「箇条書きが 3 行以上」「箇条書きが本体
/// (それ以外の行より多い)」の 3 つを満たしたときだけ。散文の多い
/// README を掴まないための線引きで、掴まなければ素の git のまま。
fn detect_journal(t: &[&str]) -> Option<Plan> {
    let (mut heads, mut bullets, mut other) = (0usize, 0usize, 0usize);
    let mut fenced = false;
    for line in t {
        let s = line.trim();
        if s.is_empty() {
            continue;
        }
        if s.starts_with("```") {
            fenced = !fenced;
            other += 1;
            continue;
        }
        if fenced {
            other += 1;
            continue;
        }
        if s.starts_with('#') && s.trim_start_matches('#').starts_with(' ') {
            heads += 1;
        } else if s.starts_with("- ") || s.starts_with("* ") || s.starts_with("+ ") {
            bullets += 1;
        } else {
            other += 1;
        }
    }
    if fenced || heads == 0 || bullets < MIN_ITEMS || bullets < other {
        return None;
    }
    Some(Plan {
        kind: ListKind::Journal,
        check: Check::Off,
        blocks: vec![(0, t.len())],
    })
}

/// TOML: `[section]` が 1 つ以上あり、残りが全部 `key = value` / コメント。
///
/// 複数行の配列 (`members = [` で改行) があるファイルは掴まない
/// (開いた行が 1 要素にならないため)。掴まなければ素の git のまま。
fn detect_toml(t: &[&str]) -> Option<Plan> {
    let mut heads: Vec<usize> = Vec::new();
    let mut entries = 0usize;
    for (i, line) in t.iter().enumerate() {
        let s = line.trim();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }
        if s.starts_with('[') && s.ends_with(']') && balanced(s) && line.starts_with('[') {
            heads.push(i);
            continue;
        }
        if !toml_entry(s) {
            return None;
        }
        entries += 1;
    }
    if heads.is_empty() || entries == 0 {
        return None;
    }
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    if heads[0] > 0 {
        blocks.push((0, heads[0]));
    }
    for (k, &h) in heads.iter().enumerate() {
        let end = heads.get(k + 1).copied().unwrap_or(t.len());
        blocks.push((h + 1, end));
    }
    Some(Plan {
        kind: ListKind::TomlSection,
        check: Check::Toml,
        blocks,
    })
}

/// `key = value` 1 行ぶん。
fn toml_entry(s: &str) -> bool {
    let Some(eq) = s.find('=') else { return false };
    let key = s[..eq].trim();
    if key.is_empty() || key.contains('=') {
        return false;
    }
    if !key
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '"' | '\''))
    {
        return false;
    }
    balanced(s) && !matches!(s.chars().last(), Some(',' | '\\' | '[' | '{' | '('))
}

/// JSON: **本物のパーサで読めたときだけ**。ブロックは「中身が全部 1 行」の本体。
fn detect_json(t: &[&str]) -> Option<Plan> {
    let joined = t.join("\n");
    let head = joined.trim_start();
    if !(head.starts_with('{') || head.starts_with('[')) {
        return None;
    }
    if !json_ok(&joined) {
        return None;
    }
    let blocks = flat_bodies(t, 2, false);
    if blocks.is_empty() {
        return None;
    }
    Some(Plan {
        kind: ListKind::Bracket,
        check: Check::Json,
        blocks,
    })
}

/// コードの配列 / 構造体リテラル。**全要素が `,` で終わる本体だけ**を対象にする。
///
/// `,` を必須にしてあるのは、末尾に足しても構文が絶対に壊れないため
/// (最後の要素に `,` が無い本体は、そこへ足すと壊れるので掴まない)。
/// 開き括弧を `[` `{` に限るのは、関数呼び出しの引数 (`(`) を
/// 「一覧」と誤認しないため — 引数の追加は本物の衝突である。
fn detect_bracket(t: &[&str]) -> Option<Plan> {
    let blocks = flat_bodies(t, MIN_ITEMS, true);
    if blocks.is_empty() {
        return None;
    }
    Some(Plan {
        kind: ListKind::Bracket,
        check: Check::Brackets,
        blocks,
    })
}

/// コメント行か。**コメントの中身は自由文でよい**。
///
/// この例外が無いと、実在する `.gitignore` はほぼ全部落ちる
/// (このリポジトリの `.gitignore` は 111 行中 40 行以上が日本語の説明で、
///  `(誤コミット防止)` や `〜しない。` を含むため「散文」と判定されていた)。
/// コメントを両側から足し合っても壊れるものは無い。
fn comment_line(s: &str) -> bool {
    s.starts_with('#') || s.starts_with("//") || s.starts_with(';')
}

/// 1 行 1 要素だけで出来たファイル (`.gitignore` / `requirements.txt` / `go.sum`)。
fn detect_flat(t: &[&str]) -> Option<Plan> {
    // シェバンがあれば**スクリプト**。命令の並びは一覧ではない
    // (順序に意味があるので、両方残すと壊れる)。
    if t.first().map(|l| l.starts_with("#!")).unwrap_or(false) {
        return None;
    }
    let mut items = 0usize;
    let mut ind: Option<&str> = None;
    for line in t {
        let s = line.trim();
        if s.is_empty() {
            continue;
        }
        let i = indent_of(line);
        match ind {
            None => ind = Some(i),
            Some(p) if p == i => {}
            _ => return None,
        }
        if comment_line(s) {
            continue;
        }
        if !flat_item(s) {
            return None;
        }
        items += 1;
    }
    if items < MIN_ITEMS {
        return None;
    }
    Some(Plan {
        kind: ListKind::Flat,
        check: Check::Off,
        blocks: vec![(0, t.len())],
    })
}

/// `use` / `mod` / `import` などが 3 行以上続くブロックだけを対象にする。
fn detect_imports(t: &[&str]) -> Option<Plan> {
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    let mut ind = "";
    for (i, line) in t.iter().enumerate() {
        let s = line.trim();
        let ok = !s.is_empty() && import_line(s);
        let same = ok && start.is_some() && ind == indent_of(line);
        if same {
            continue;
        }
        if let Some(st) = start.take() {
            if i - st >= MIN_ITEMS {
                blocks.push((st, i));
            }
        }
        if ok {
            start = Some(i);
            ind = indent_of(line);
        }
    }
    if let Some(st) = start {
        if t.len() - st >= MIN_ITEMS {
            blocks.push((st, t.len()));
        }
    }
    if blocks.is_empty() {
        return None;
    }
    Some(Plan {
        kind: ListKind::Imports,
        check: Check::Off,
        blocks,
    })
}

/// 宣言 1 行ぶん。可視性の接頭辞は剥がしてから見る。
fn import_line(s: &str) -> bool {
    if !balanced(s) {
        return false;
    }
    let mut rest = s;
    for p in ["pub(crate) ", "pub(super) ", "pub "] {
        if let Some(r) = rest.strip_prefix(p) {
            rest = r.trim_start();
            break;
        }
    }
    const HEADS: &[&str] = &[
        "use ",
        "mod ",
        "extern crate ",
        "import ",
        "export ",
        "from ",
        "require ",
        "require(",
        "using ",
        "#include ",
        "@import ",
    ];
    if !HEADS.iter().any(|h| rest.starts_with(h)) {
        return false;
    }
    matches!(s.chars().last(), Some(';' | '"' | '\'' | ')' | '>'))
}

/// 「開いて閉じるまでの中身が全部 1 行」の本体を集める。
///
/// 入れ子の本体があると外側は候補から落ちる (中に 1 行で閉じない行が
/// あるため)。結果として**ブロックは決して重ならない**。
fn flat_bodies(t: &[&str], min_body: usize, require_comma: bool) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut open: Option<usize> = None;
    let mut ok = true;
    let mut count = 0usize;
    let mut ind: Option<String> = None;
    for (i, raw) in t.iter().enumerate() {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        let (d, lo) = line_delta(raw);
        if d == 1 && lo >= 0 && matches!(s.chars().last(), Some('{' | '[')) {
            open = Some(i + 1);
            ok = true;
            count = 0;
            ind = None;
            continue;
        }
        if d == -1 && lo < 0 {
            if let Some(st) = open.take() {
                if ok && count >= min_body && i >= st {
                    out.push((st, i));
                }
            }
            continue;
        }
        if open.is_some() {
            if d != 0 || lo < 0 || (require_comma && !s.ends_with(',')) {
                ok = false;
            }
            match &ind {
                None => ind = Some(indent_of(raw).to_string()),
                Some(p) if p == indent_of(raw) => {}
                _ => ok = false,
            }
            count += 1;
        }
    }
    out
}

// ── ブロック → 領域表 ──────────────────────────────────────────

/// 自動判定のブロックから [`Regions`] を作る。
///
/// **ブロックの外は全部マーカ扱い**にするので、区切り行 (`}` や
/// `[section]` の見出し) が union のチャンクへ紛れ込むことはあり得ない。
fn regions_from_blocks(n: usize, blocks: &[(usize, usize)]) -> Regions {
    let mut gap = vec![None; n + 1];
    let mut marker = vec![true; n];
    for (id, &(s, e)) in blocks.iter().enumerate() {
        if s > e || e > n {
            return Regions {
                gap: vec![None; n + 1],
                marker: vec![true; n],
                count: 0,
                balanced: false,
            };
        }
        for g in gap.iter_mut().take(e + 1).skip(s) {
            if g.is_some() {
                // 重なった = 判定が壊れている。union を一切効かせない。
                return Regions {
                    gap: vec![None; n + 1],
                    marker: vec![true; n],
                    count: 0,
                    balanced: false,
                };
            }
            *g = Some(id);
        }
        for m in marker.iter_mut().take(e).skip(s) {
            *m = false;
        }
    }
    Regions {
        gap,
        marker,
        count: blocks.len(),
        balanced: true,
    }
}

// ── キー — 「同じキーで値が違えば衝突」を実装する ───────────────

/// その行の**キー**。両側が同じキーの行を足したら、それは追記ではなく衝突。
///
/// `::` を跨がないのが肝で、`BindAction::Save,` の `Action` を
/// キーと読むと、列挙子を足しただけの追記が全部衝突になる。
fn entry_key(t: &str, kind: ListKind) -> Option<String> {
    let s = t.trim();
    if s.is_empty() {
        return None;
    }
    if kind == ListKind::Journal {
        let h = s.trim_start_matches('#');
        if h.len() == s.len() {
            return None;
        }
        let h = h.trim();
        return (!h.is_empty()).then(|| format!("#{h}"));
    }
    // "key": value — JSON / 引用符付きの YAML
    if let Some(rest) = s.strip_prefix('"') {
        let q = rest.find('"')?;
        let k = &rest[..q];
        let after = rest[q + 1..].trim_start();
        if after.starts_with(':') && !after.starts_with("::") && !k.is_empty() {
            return Some(k.to_string());
        }
        return None;
    }
    if let Some(k) = decl_key(s) {
        return Some(k);
    }
    // key = value / key: value
    let mut end = 0usize;
    for (i, c) in s.char_indices() {
        if c.is_alphanumeric() || matches!(c, '_' | '.' | '-' | '/' | '@') {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    let rest = s[end..].trim_start();
    match rest.chars().next() {
        Some('=') => Some(s[..end].to_string()),
        Some(':') if !rest.starts_with("::") => Some(s[..end].to_string()),
        _ => None,
    }
}

/// `mod x;` / `use a::b;` / `extern crate c;` の名前。
fn decl_key(s: &str) -> Option<String> {
    let mut rest = s;
    for p in ["pub(crate) ", "pub(super) ", "pub "] {
        if let Some(r) = rest.strip_prefix(p) {
            rest = r.trim_start();
            break;
        }
    }
    for kw in ["mod ", "use ", "extern crate "] {
        if let Some(r) = rest.strip_prefix(kw) {
            let name = r.trim_end_matches(';').trim();
            if name.is_empty() || name.contains(char::is_whitespace) {
                return None;
            }
            return Some(format!("{}:{name}", kw.trim()));
        }
    }
    None
}

/// 2 回以上出てくるキーの集合。**`BTreeSet` なので反復順が出力へ漏れない。**
fn dup_keys(lines: &[Line], kind: ListKind) -> BTreeSet<String> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for l in lines {
        if let Some(k) = entry_key(&l.text, kind) {
            *seen.entry(k).or_default() += 1;
        }
    }
    seen.into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(k, _)| k)
        .collect()
}

// ── 構文検査 — 「壊れたものは返さない」 ─────────────────────────

/// 出来上がりを検査する。**落ちたら結果ごと捨てて素の git へ降りる。**
fn check_ok(p: &Plan, b: &[Line], o: &[Line], t: &[Line], out: &[Line], text: &str) -> bool {
    // 1. 重複キー。**元から重複していたものだけ**許す (こちらが増やしたら駄目)。
    let mut allowed = dup_keys(b, p.kind);
    allowed.append(&mut dup_keys(o, p.kind));
    allowed.append(&mut dup_keys(t, p.kind));
    if !dup_keys(out, p.kind).iter().all(|k| allowed.contains(k)) {
        return false;
    }
    match p.check {
        Check::Off => true,
        Check::Json => json_ok(text),
        Check::Toml => toml_ok(text),
        Check::Brackets => file_delta(out) == file_delta(b),
    }
}

/// ファイル全体の括弧の増減と最小値。
fn file_delta(lines: &[Line]) -> (i32, i32) {
    let (mut d, mut lo) = (0i32, 0i32);
    for l in lines {
        let (dd, ll) = line_delta(&l.text);
        lo = lo.min(d + ll);
        d += dd;
    }
    (d, lo)
}

/// JSON として読み直す。**同じオブジェクトに同じキーが 2 つあったら不正**とする
/// (規格上は許されるが、依存表としては壊れているため)。
fn json_ok(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0usize;
    if !json_value(b, &mut i, 0) {
        return false;
    }
    json_ws(b, &mut i);
    i == b.len()
}

fn json_ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\n' | b'\r') {
        *i += 1;
    }
}

fn json_value(b: &[u8], i: &mut usize, depth: u32) -> bool {
    if depth > 128 {
        return false;
    }
    json_ws(b, i);
    let Some(&c) = b.get(*i) else { return false };
    match c {
        b'{' => json_object(b, i, depth),
        b'[' => json_array(b, i, depth),
        b'"' => json_string(b, i).is_some(),
        b't' => json_lit(b, i, b"true"),
        b'f' => json_lit(b, i, b"false"),
        b'n' => json_lit(b, i, b"null"),
        _ => json_number(b, i),
    }
}

fn json_lit(b: &[u8], i: &mut usize, w: &[u8]) -> bool {
    if b.len() >= *i + w.len() && &b[*i..*i + w.len()] == w {
        *i += w.len();
        true
    } else {
        false
    }
}

fn json_number(b: &[u8], i: &mut usize) -> bool {
    let st = *i;
    if b.get(*i) == Some(&b'-') {
        *i += 1;
    }
    while matches!(b.get(*i), Some(c) if c.is_ascii_digit()) {
        *i += 1;
    }
    if b.get(*i) == Some(&b'.') {
        *i += 1;
        while matches!(b.get(*i), Some(c) if c.is_ascii_digit()) {
            *i += 1;
        }
    }
    if matches!(b.get(*i), Some(b'e' | b'E')) {
        *i += 1;
        if matches!(b.get(*i), Some(b'+' | b'-')) {
            *i += 1;
        }
        while matches!(b.get(*i), Some(c) if c.is_ascii_digit()) {
            *i += 1;
        }
    }
    *i > st
}

fn json_string(b: &[u8], i: &mut usize) -> Option<String> {
    if b.get(*i) != Some(&b'"') {
        return None;
    }
    *i += 1;
    let st = *i;
    while let Some(&c) = b.get(*i) {
        match c {
            b'\\' => *i += 2,
            b'"' => {
                let s = String::from_utf8_lossy(&b[st..*i]).into_owned();
                *i += 1;
                return Some(s);
            }
            _ => *i += 1,
        }
    }
    None
}

fn json_object(b: &[u8], i: &mut usize, depth: u32) -> bool {
    *i += 1; // '{'
    let mut keys: BTreeSet<String> = BTreeSet::new();
    json_ws(b, i);
    if b.get(*i) == Some(&b'}') {
        *i += 1;
        return true;
    }
    loop {
        json_ws(b, i);
        let Some(k) = json_string(b, i) else {
            return false;
        };
        if !keys.insert(k) {
            return false; // 同じキーが 2 つ = 壊れている
        }
        json_ws(b, i);
        if b.get(*i) != Some(&b':') {
            return false;
        }
        *i += 1;
        if !json_value(b, i, depth + 1) {
            return false;
        }
        json_ws(b, i);
        match b.get(*i) {
            Some(b',') => *i += 1,
            Some(b'}') => {
                *i += 1;
                return true;
            }
            _ => return false,
        }
    }
}

fn json_array(b: &[u8], i: &mut usize, depth: u32) -> bool {
    *i += 1; // '['
    json_ws(b, i);
    if b.get(*i) == Some(&b']') {
        *i += 1;
        return true;
    }
    loop {
        if !json_value(b, i, depth + 1) {
            return false;
        }
        json_ws(b, i);
        match b.get(*i) {
            Some(b',') => *i += 1,
            Some(b']') => {
                *i += 1;
                return true;
            }
            _ => return false,
        }
    }
}

/// TOML の最小検査。**セクション毎の重複キー**を弾く (依存表の壊れ方はこれ)。
fn toml_ok(s: &str) -> bool {
    let mut section = String::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for raw in s.split_inclusive('\n') {
        let line = raw.trim_end_matches(['\n', '\r']);
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if t.starts_with('[') && t.ends_with(']') && balanced(t) {
            section = t.to_string();
            continue;
        }
        if !toml_entry(t) {
            return false;
        }
        let key = t[..t.find('=').unwrap_or(0)].trim().to_string();
        if !seen.insert((section.clone(), key)) {
            return false;
        }
    }
    true
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
    /// マーカが無いとき、**中身から一覧を探す**。既定は off。
    /// `merge=zaivern-union-auto` (= `--auto`) を付けたときだけ立つ。
    /// マーカがあればそちらが勝つ (明示指定は自動判定より強い)。
    pub auto: bool,
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
            auto: false,
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
    // 自動判定はマーカが 1 つも無いときだけ。**明示指定は自動判定より強い。**
    let plans = if opts.auto && !opts.whole_file && !has_marker(base, ours, theirs) {
        plans_for(&b, &o, &t)
    } else {
        None
    };
    let (lines, conflicts) = three_way(&b, &o, &t, opts, dom, plans.as_ref());
    let text = join_lines(&lines, dom);
    if conflicts > 0 {
        return Resolution::Conflict(text);
    }
    if let Some((pb, _, _)) = &plans {
        // **構文を壊したら結果ごと捨てる。** 自動判定でしか通らない道なので、
        // マーカを書いた人の結果は 1 バイトも変わらない。
        if !check_ok(pb, &b, &o, &t, &lines, &text) {
            let (l2, c2) = three_way(&b, &o, &t, opts, dom, None);
            let t2 = join_lines(&l2, dom);
            return if c2 == 0 {
                Resolution::Merged(t2)
            } else {
                Resolution::Conflict(t2)
            };
        }
    }
    Resolution::Merged(text)
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
    plans: Option<&(Plan, Plan, Plan)>,
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

    let (rb, ro, rt) = match plans {
        Some((pb, po, pt)) => (
            regions_from_blocks(base.len(), &pb.blocks),
            regions_from_blocks(ours.len(), &po.blocks),
            regions_from_blocks(theirs.len(), &pt.blocks),
        ),
        None => (
            regions_of(base, opts.whole_file),
            regions_of(ours, opts.whole_file),
            regions_of(theirs, opts.whole_file),
        ),
    };
    // **片側でもマーカを触っていたら union は一切効かせない。**
    let union_ok = rb.balanced
        && ro.balanced
        && rt.balanced
        && rb.count > 0
        && rb.count == ro.count
        && ro.count == rt.count;
    let kind = plans.map(|(pb, _, _)| pb.kind);
    let dedup = kind.map(ListKind::dedup).unwrap_or(Dedup::Fold);

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
                kind,
                dedup,
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
            kind,
            dedup,
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
    /// 自動判定で決まった種類 (マーカ方式なら `None`)。
    kind: Option<ListKind>,
    /// 重複行の扱い。種類ごとに決まる。
    dedup: Dedup,
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
                union_merge(b, o, t, cx.opts, cx.kind, cx.dedup)
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
///
/// **同じキーの行を両側が足していて中身が違えば `None`** (それは追記ではなく
/// 衝突である)。`dedup` が [`Dedup::Keep`] のときは同じキーというだけで降りる
/// — CHANGELOG に同じ見出しが 2 つ並ぶのは、人が直すべき状態だから。
fn union_merge(
    b: &[Line],
    o: &[Line],
    t: &[Line],
    opts: &UnionOpts,
    kind: Option<ListKind>,
    dedup: Dedup,
) -> Option<Vec<Line>> {
    let (o_ins, o_anchor) = align(b, o)?;
    let (t_ins, _) = align(b, t)?;
    let kk = kind.unwrap_or(ListKind::Flat);
    // 両ブランチが**同じ行**を足した場合は 1 本にまとめる。空白だけの行は
    // 区切りとして何度も出てくるのが普通なので、畳まない。
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut o_keys: BTreeMap<String, &str> = BTreeMap::new();
    for slot in &o_ins {
        for l in slot {
            if !l.text.trim().is_empty() {
                seen.insert(l.text.as_str());
            }
            if let Some(k) = entry_key(&l.text, kk) {
                o_keys.insert(k, l.text.as_str());
            }
        }
    }
    for slot in &t_ins {
        for l in slot {
            let Some(k) = entry_key(&l.text, kk) else {
                continue;
            };
            if let Some(prev) = o_keys.get(&k) {
                if dedup == Dedup::Keep || *prev != l.text.as_str() {
                    return None;
                }
            }
        }
    }
    let mut out: Vec<Line> = Vec::new();
    for k in 0..=b.len() {
        let mut block: Vec<Line> = o_ins[k].clone();
        for l in &t_ins[k] {
            if dedup == Dedup::Fold && !l.text.trim().is_empty() && seen.contains(l.text.as_str()) {
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
            "--auto" => opts.auto = true,
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
    // **マーカが 1 つも無いファイルでは、自動判定が確信できたときしか触らない。**
    // どちらも当たらなければ git 本体へ委譲するので、「ドライバを入れたら
    // 普段のマージまで変わった」が構造的に起こらない。
    let markers = has_marker(&bs, &os, &ts);
    let auto_only = opts.auto && !opts.whole_file && !markers;
    if !opts.whole_file && !markers && !(opts.auto && auto_applies(&bs, &os, &ts)) {
        return delegate_to_git(o, a, b, opts.marker_size);
    }

    let res = resolve(&bs, &os, &ts, &opts);
    // 自動判定モードは**全部解決できたときだけ**結果を差し替える。1 つでも
    // 衝突が残るなら書き込む前に git 本体へ降りるので、こちらの衝突マーカが
    // 画面に出ることは一度も無い (= 見た目が変わらない)。**`%A` へ書く前に
    // 降りるのが肝** — 書いてしまうと `git merge-file` が読む ours が壊れる。
    if auto_only && res.has_conflict() {
        return delegate_to_git(o, a, b, opts.marker_size);
    }
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
使い方: zai merge-driver [--auto] [--whole] [--sorted] <base> <ours> <theirs> [マーカ長] [元のパス]

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

/// `.gitattributes` へ書く 1 行と、**そう決めた根拠**。
///
/// 根拠を持ち歩くのは、画面に「なぜこのパターンなのか」を出すため。
/// 「ツールが勝手に書いた意味の分からない行」は必ず消される。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttrLine {
    /// git のパターン (`.gitignore` / `*.toml` / `docs/list.txt`)。
    pub pattern: String,
    /// 当てるドライバ名。
    pub driver: String,
    /// 何を見てそう決めたか。
    pub why: String,
    /// このパターンに実際に当たった追跡ファイル数。
    pub files: usize,
    /// 既存の指定と衝突しないか調べるときの代表パス。
    pub sample: String,
}

impl AttrLine {
    /// `.gitattributes` の 1 行 (改行は含まない)。
    pub fn line(&self) -> String {
        format!("{} merge={}", self.pattern, self.driver)
    }
}

/// 導入 / 解除の結果。**何をしたかを数で返す。**
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// 実際に触ったリポジトリのルート。
    pub root: PathBuf,
    /// 登録 (または解除) したドライバの数。
    pub drivers: usize,
    /// `.gitattributes` へ書いた行。
    pub added: Vec<AttrLine>,
    /// **既に別の merge 指定があるので触らなかった**行。
    pub skipped: Vec<AttrLine>,
    /// 画面へそのまま出す 1 行。
    pub message: String,
}

/// `.gitattributes` の**提案**に出してよい判定か。
///
/// 提案に載せるということは「このパターンのマージを全部こちらのバイナリへ
/// 通す」ということなので、**ファイル全体が一覧**だと言い切れるものだけに
/// 絞る。ブロック単位の判定 (`Imports` / コードの配列リテラル) は、
/// 中身の 3 行だけを根拠に `*.rs` のような広いパターンを書くことになり、
/// 効果に比べて影響範囲が大きすぎる (実測: このリポジトリでは 124 個の
/// `.rs` のうち 102 個が該当してしまい `*.rs` が提案された)。
/// **明示的に `.gitattributes` へ書けば今までどおり効く。**
fn suggestable(p: &Plan) -> bool {
    matches!(
        (p.kind, p.check),
        (ListKind::Flat, _)
            | (ListKind::Journal, _)
            | (ListKind::TomlSection, _)
            | (ListKind::Bracket, Check::Json)
    )
}

/// リポジトリを実際に見て、`.gitattributes` に足すべき行を起こす。
///
/// * **存在するファイルだけ**を対象にする (使われていないパターンを並べない)。
/// * 判定は中身 ([`detect`])。拡張子はここで**パターンをまとめる**ときにしか
///   使わない。対象になっても中身が一覧でなければ何も起きないので、
///   まとめすぎても安全側に倒れる。
/// * 既に別の merge ドライバが当たっているパターンは**返さない**
///   (判定は git 自身の `check-attr` にやらせるので、glob の解釈がずれない)。
/// * 並びは `BTreeMap` 由来で決定的。
pub fn suggest_attributes(repo_root: &Path) -> Vec<AttrLine> {
    let Ok(root) = crate::worktree::repo_root(repo_root) else {
        return Vec::new();
    };
    let listed = git_out(&root, &["ls-files"]).unwrap_or_default();
    let mut ext_read: BTreeMap<String, usize> = BTreeMap::new();
    let mut hits: Vec<(String, ListKind)> = Vec::new();
    let mut read = 0usize;
    for rel in listed.lines().filter(|l| !l.is_empty()) {
        if read >= MAX_SUGGEST_READ {
            break;
        }
        let ext = Path::new(rel)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_string();
        let full = root.join(rel);
        let Ok(meta) = std::fs::metadata(&full) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_SUGGEST_BYTES {
            continue;
        }
        read += 1;
        if !ext.is_empty() {
            *ext_read.entry(ext).or_default() += 1;
        }
        let Ok(text) = std::fs::read_to_string(&full) else {
            continue;
        };
        let Some(plan) = detect_lines(&split_lines(&text)) else {
            continue;
        };
        if !suggestable(&plan) {
            continue;
        }
        hits.push((rel.to_string(), plan.kind));
    }

    // まとめ方は**狭い順**: よく知られた名前 → 同じファイル名 → 拡張子 → 個別。
    // 1 つのファイルは 1 つのパターンにしか入らない。
    let mut out: Vec<AttrLine> = Vec::new();
    let base_of = |rel: &str| {
        Path::new(rel)
            .file_name()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_string()
    };
    let mut by_base: BTreeMap<String, Vec<(String, ListKind)>> = BTreeMap::new();
    for h in hits {
        by_base.entry(base_of(&h.0)).or_default().push(h);
    }
    let mut rest: Vec<(String, ListKind)> = Vec::new();
    for (base, group) in by_base {
        // よく知られた名前、または同じ名前が 2 つ以上あるならファイル名でまとめる。
        if WELL_KNOWN.contains(&base.as_str()) || group.len() >= 2 {
            out.push(AttrLine {
                pattern: base,
                driver: AUTO_DRIVER.to_string(),
                why: trf(
                    "{k} を {n} ファイルで確認",
                    &[
                        ("k", group[0].1.label().to_string()),
                        ("n", group.len().to_string()),
                    ],
                ),
                files: group.len(),
                sample: group[0].0.clone(),
            });
        } else {
            rest.extend(group);
        }
    }
    let mut by_ext: BTreeMap<String, Vec<(String, ListKind)>> = BTreeMap::new();
    let mut singles: Vec<(String, ListKind)> = Vec::new();
    for h in rest {
        let ext = Path::new(&h.0)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_string();
        if ext.is_empty() {
            singles.push(h);
        } else {
            by_ext.entry(ext).or_default().push(h);
        }
    }
    for (ext, group) in by_ext {
        let total = ext_read.get(&ext).copied().unwrap_or(group.len());
        // 「その拡張子の過半数が一覧」ならまとめる。そうでなければ個別に出す。
        if group.len() >= 2 && group.len() * 2 >= total {
            out.push(AttrLine {
                pattern: format!("*.{ext}"),
                driver: AUTO_DRIVER.to_string(),
                why: trf(
                    "{k} が {n}/{t} ファイル",
                    &[
                        ("k", group[0].1.label().to_string()),
                        ("n", group.len().to_string()),
                        ("t", total.to_string()),
                    ],
                ),
                files: group.len(),
                sample: group[0].0.clone(),
            });
        } else {
            singles.extend(group);
        }
    }
    singles.sort();
    for (rel, kind) in singles.into_iter().take(MAX_SUGGEST_PATHS) {
        out.push(AttrLine {
            pattern: rel.clone(),
            driver: AUTO_DRIVER.to_string(),
            why: trf("{k}", &[("k", kind.label().to_string())]),
            files: 1,
            sample: rel,
        });
    }
    out.sort_by(|a, b| a.pattern.cmp(&b.pattern));
    out
}

/// 既に別の merge ドライバが当たっている行を分ける。
///
/// 判定は `git check-attr` に任せる。**glob を自前で書くと必ずずれる**ので、
/// git 自身に「このパスにはどの merge が当たるか」を聞く。
fn split_existing(root: &Path, want: Vec<AttrLine>) -> (Vec<AttrLine>, Vec<AttrLine>) {
    let (mut keep, mut skip) = (Vec::new(), Vec::new());
    for a in want {
        let probe = if a.sample.is_empty() {
            a.pattern.clone()
        } else {
            a.sample.clone()
        };
        let cur = git_out(root, &["check-attr", "merge", "--", &probe]).unwrap_or_default();
        let value = cur.rsplit_once(": ").map(|(_, v)| v.trim()).unwrap_or("");
        if value.is_empty()
            || value == "unspecified"
            || value == "unset"
            || value.starts_with("zaivern-union")
        {
            keep.push(a);
        } else {
            skip.push(a);
        }
    }
    (keep, skip)
}

/// このリポジトリへドライバを登録し、`.gitattributes` の管理ブロックを書く。
pub fn install(repo: &Path) -> Result<Report, String> {
    let exe = std::env::current_exe()
        .map_err(|e| trf("実行ファイルの場所が分かりません: {e}", &[("e", e.to_string())]))?;
    install_with(repo, &exe, configured_patterns(repo).as_deref())
}

/// [`install`] の中身。実行ファイルとパターンを外から渡せる形
/// (テストがビルド済みバイナリの場所を差し替えられるようにするため)。
///
/// `patterns` が `None` なら [`suggest_attributes`] にリポジトリを見て
/// 決めさせる。**何度呼んでも同じ結果になる** (管理ブロックは 1 つだけ)。
pub fn install_with(
    repo: &Path,
    exe: &Path,
    patterns: Option<&[String]>,
) -> Result<Report, String> {
    let root = crate::worktree::repo_root(repo)?;
    for (name, flags) in DRIVERS {
        let key_name = format!("merge.{name}.name");
        let key_drv = format!("merge.{name}.driver");
        let cmd = driver_command(exe, flags);
        git_out(&root, &["config", "--local", &key_name, DRIVER_DESC])?;
        git_out(&root, &["config", "--local", &key_drv, &cmd])?;
    }
    let want: Vec<AttrLine> = match patterns {
        Some(ps) => ps
            .iter()
            .map(|p| AttrLine {
                pattern: p.clone(),
                driver: AUTO_DRIVER.to_string(),
                why: tr("設定 union.patterns の指定"),
                files: 0,
                sample: p.clone(),
            })
            .collect(),
        None => suggest_attributes(&root),
    };
    let (added, skipped) = split_existing(&root, want);
    write_attributes(&root, &added)?;
    let message = trf(
        "追記の自動マージを導入しました (パターン {n} 件 / 既存の指定があるので見送り {s} 件)。",
        &[("n", added.len().to_string()), ("s", skipped.len().to_string())],
    );
    Ok(Report {
        root,
        drivers: DRIVERS.len(),
        added,
        skipped,
        message,
    })
}

/// 登録を解除し、`.gitattributes` の管理ブロックを取り除く。
///
/// **入れたものを綺麗に戻せないツールは信用されない。** こちらが作った
/// `.gitattributes` は消し、人が書いた行は 1 つも触らない。
pub fn uninstall(repo: &Path) -> Result<Report, String> {
    let root = crate::worktree::repo_root(repo)?;
    for (name, _) in DRIVERS {
        let section = format!("merge.{name}");
        // 未登録でも失敗にしない (解除は何度呼んでも同じ結果にする)。
        let _ = git_out(&root, &["config", "--local", "--remove-section", &section]);
    }
    strip_attributes(&root)?;
    Ok(Report {
        root,
        drivers: DRIVERS.len(),
        added: Vec::new(),
        skipped: Vec::new(),
        message: tr("追記の自動マージを解除しました。"),
    })
}

/// このリポジトリにドライバが登録済みか。
pub fn is_installed(repo: &Path) -> bool {
    git_out(repo, &["config", "--local", "--get", "merge.zaivern-union.driver"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// 設定 `union.patterns` の指定。**空なら `None`** = リポジトリを見て決める。
fn configured_patterns(repo: &Path) -> Option<Vec<String>> {
    let cfg = crate::config::load(std::slice::from_ref(&repo.to_path_buf()), false);
    let raw = cfg.feature_str("union.patterns");
    if raw.trim().is_empty() {
        return None;
    }
    Some(split_patterns(&raw))
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

/// 管理ブロックを書き直す。**改行は元のファイルに合わせる** (CRLF の
/// リポジトリで 1 行だけ LF になると、次の人の差分が全行になる)。
fn write_attributes(root: &Path, lines: &[AttrLine]) -> Result<(), String> {
    let path = root.join(".gitattributes");
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let (mut kept, eol) = strip_block(&old);
    if lines.is_empty() {
        // 書くものが無いなら空のブロックを置かない (空白は作らない)。
        if kept.trim().is_empty() {
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        return std::fs::write(&path, kept)
            .map_err(|e| trf(".gitattributes を書けません: {e}", &[("e", e.to_string())]));
    }
    if !kept.is_empty() && !kept.ends_with('\n') {
        kept.push_str(eol);
    }
    kept.push_str(ATTR_BEGIN);
    kept.push_str(eol);
    for a in lines {
        kept.push_str(&a.line());
        kept.push_str(eol);
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
    /// まだ導入していないときに「何を対象にするつもりか」を先に見せる。
    suggest: Vec<AttrLine>,
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
    /// ファイル内の領域の数。0 なら「マーカは無い」。
    regions: usize,
    /// マーカが無いときに自動判定が見つけた種類。**両方無ければ素の git と同じ。**
    kind: Option<ListKind>,
}

/// 一度に `git check-attr` へ渡すパス数 (コマンド行の上限を避ける)。
const ATTR_CHUNK: usize = 128;
/// 走査するファイル数の上限。
const MAX_FILES: usize = 5000;
/// 領域数を数えるために中身を読むファイル数の上限。
const MAX_READ: usize = 300;
/// 再走査の基準間隔 (実測に応じて `git::scan_interval` が伸ばす)。
const SCAN_BASE: Duration = Duration::from_secs(4);

fn scan(repo: &Path, want_suggest: bool) -> Snapshot {
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
                kind: None,
            });
        }
    }
    snap.files.sort_by(|a, b| a.path.cmp(&b.path));
    for f in snap.files.iter_mut().take(MAX_READ) {
        if let Ok(text) = std::fs::read_to_string(root.join(&f.path)) {
            f.regions = text.lines().filter(|l| l.contains(BEGIN)).count();
            if f.regions == 0 {
                f.kind = detect(&text);
            }
        }
    }
    // 未導入のときだけ「何が対象になるか」を先に出す (導入後は上の一覧が答え)。
    // **走査のたびには数えない** — 全ファイルを読むので実測 2.1 秒 (debug,
    // 248 ファイル) かかる。開いた最初の 1 回だけ計算して以後は使い回す。
    if want_suggest && !snap.installed {
        snap.suggest = suggest_attributes(&root);
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
    action: Option<Report>,
    /// 失敗したときの文言。
    action_err: Option<String>,
    action_rx: Option<Receiver<Result<Report, String>>>,
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

fn spawn_scan(root: PathBuf, want_suggest: bool) -> Receiver<Snapshot> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(scan(&root, want_suggest));
    });
    rx
}

fn spawn_action(root: PathBuf, install_it: bool) -> Receiver<Result<Report, String>> {
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
                match r {
                    Ok(rep) => {
                        st.action = Some(rep);
                        st.action_err = None;
                    }
                    Err(e) => {
                        st.action = None;
                        st.action_err = Some(e);
                    }
                }
                st.action_rx = None;
                st.last_scan = None; // 変えたので取り直す
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => st.action_rx = None,
        }
    }
    if let Some(rx) = &st.pending {
        match rx.try_recv() {
            Ok(mut s) => {
                st.last_cost = Some(s.cost);
                if s.suggest.is_empty() {
                    // 今回計算していないなら、前回の提案をそのまま残す。
                    s.suggest = std::mem::take(&mut st.snap.suggest);
                }
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
            let want = st.snap.suggest.is_empty();
            st.pending = Some(spawn_scan(st.root.clone(), want));
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
                st.action_err = None;
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
    if let Some(r) = &st.action {
        ui.label(egui::RichText::new(&r.message).color(dim));
        if !r.added.is_empty() {
            ui.label(
                egui::RichText::new(
                    r.added
                        .iter()
                        .map(|a| a.pattern.clone())
                        .collect::<Vec<_>>()
                        .join(" "),
                )
                .color(dim),
            );
        }
        for a in &r.skipped {
            ui.label(
                egui::RichText::new(trf(
                    "{p} は既に別の merge 指定があるので触っていません",
                    &[("p", a.pattern.clone())],
                ))
                .color(dim),
            );
        }
    }
    if let Some(e) = &st.action_err {
        ui.label(egui::RichText::new(e).color(ui.visuals().warn_fg_color));
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
                    } else if let Some(k) = f.kind {
                        tr(k.label())
                    } else {
                        tr("—")
                    };
                    ui.put(
                        *r,
                        egui::Label::new(egui::RichText::new(txt).color(dim)).truncate(),
                    );
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
                if st.snap.suggest.is_empty() {
                    ui.label(
                        egui::RichText::new(tr(
                            "「このリポジトリに導入」を押すと、一覧への追記どうしが衝突しなくなります。",
                        ))
                        .color(ui.visuals().weak_text_color()),
                    );
                } else {
                    ui.label(
                        egui::RichText::new(trf(
                            "導入すると {n} パターンが対象になります: {p}",
                            &[
                                ("n", st.snap.suggest.len().to_string()),
                                (
                                    "p",
                                    st.snap
                                        .suggest
                                        .iter()
                                        .take(6)
                                        .map(|a| a.pattern.clone())
                                        .collect::<Vec<_>>()
                                        .join(" "),
                                ),
                            ],
                        ))
                        .color(ui.visuals().weak_text_color()),
                    );
                }
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
        label: "追記の自動マージを適用するファイル (空白区切り / 空ならリポジトリを見て決める)",
        help: "導入時に .gitattributes へ書き込むパターンです。空にしておくと、実際にリポジトリを走査して「1 行 1 要素の一覧」だと確信できたファイルだけを対象にします。対象になっても中身が一覧でなければ素の git と同じ結果のままです。",
        default: crate::feature::SettingValue::Text(""),
    }],
    binds: &[],
    ..crate::feature::Feature::DEFAULT
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
            install_with(&repo, &exe, Some(&pats)).expect("導入");
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
        install_with(&repo, &repo.join("fake-zai"), Some(&["*.md".to_string()])).expect("導入");
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
        // `--auto` などのフラグも環境変数で渡す (位置引数は libtest が食う)。
        let mut all: Vec<String> = std::env::var("ZV_UNION_FLAGS")
            .unwrap_or_default()
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();
        all.push(o);
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

    // ── 10.5 実共有面のフィクスチャ (tools/union-bench.sh と同じ題材) ──
    //
    // 合成ベンチ (`let value = N;` の書き換え) では union は効果ゼロで、
    // それは設計どおりの正しい「解決しない」。**効く条件と効かない条件を
    // 両方コードで固定する**ことで、「union は効かない」とも
    // 「union は何でも直す」とも読めないようにする。

    /// 追記だけの共有面 (config / i18n / mod 宣言 / CHANGELOG) は解決する。
    #[test]
    fn 実共有面_追記だけの一覧は四種類とも解決する() {
        let cases: &[(&str, &str, &str, &str)] = &[
            // (名前, 既存の中身, ours が足す行, theirs が足す行)
            (
                "config.rs 型",
                "    pub theme: String,\n",
                "    pub which_key_delay_ms: u64,\n",
                "    pub local_history_days: u32,\n",
            ),
            (
                "i18n テーブル",
                "    (\"save\", \"保存\"),\n",
                "    (\"open\", \"開く\"),\n",
                "    (\"quit\", \"終了\"),\n",
            ),
            ("mod 宣言一覧", "mod app;\n", "mod whichkey;\n", "mod local_history;\n"),
            (
                "CHANGELOG",
                "- 0.1.0 最初のリリース\n",
                "- 0.2.0 which-key\n",
                "- 0.2.0 local history\n",
            ),
        ];
        for (name, base_body, o_add, t_add) in cases {
            let base = wrapped(base_body);
            let ours = wrapped(&format!("{base_body}{o_add}"));
            let theirs = wrapped(&format!("{base_body}{t_add}"));
            match resolve(&base, &ours, &theirs, &opts()) {
                Resolution::Merged(out) => {
                    assert_eq!(out, wrapped(&format!("{base_body}{o_add}{t_add}")), "{name}");
                }
                Resolution::Conflict(out) => panic!("{name} は解決されるはず:\n{out}"),
            }
        }
    }

    /// **keybinds.rs 型は解決できない。** 固定長配列の長さを両側が別々の値へ
    /// 書き換えるので、これは「追記」ではなく既存行の**変更**にあたる。
    /// できないことを、できないと固定しておく。
    #[test]
    fn 実共有面_固定長配列のカウントは解決できない() {
        let head = |n: usize| format!("pub const ALL: [BindAction; {n}] = [\n");
        let tail = |n: usize| format!("];\nfn count() {{ assert_eq!(ALL.len(), {n}); }}\n");
        let body = "    BindAction::Save,\n";
        let base = format!("{}{}{}", head(1), wrapped(body), tail(1));
        // ours は 2 件、theirs は 3 件になったつもりで長さを書き換える。
        let ours = format!(
            "{}{}{}",
            head(2),
            wrapped(&format!("{body}    BindAction::Open,\n")),
            tail(2)
        );
        let theirs = format!(
            "{}{}{}",
            head(3),
            wrapped(&format!("{body}    BindAction::Quit,\n")),
            tail(3)
        );
        let Resolution::Conflict(out) = resolve(&base, &ours, &theirs, &opts()) else {
            panic!("配列長の書き換えを自動で解決してはいけない");
        };
        // 中身の追記 (領域の内側) は両方残るが、**数値行は衝突として残る**。
        assert!(out.contains("BindAction::Open"), "{out}");
        assert!(out.contains("BindAction::Quit"), "{out}");
        assert!(out.contains("<<<<<<<"), "数値行が衝突として残っていない:\n{out}");
    }

    /// **衝突ゼロ = 安全ではない。** 全員が同じ新しい長さを書くと、git も
    /// union も綺麗にマージするが、**要素数と宣言長がずれたまま通る**。
    /// これは union の欠陥ではなく (素の git でも同じ)、
    /// 「カウント検査テストを持て」という設計上の要求である。
    #[test]
    fn 実共有面_全員が同じ長さを書くと綺麗に壊れる() {
        let head = |n: usize| format!("pub const ALL: [BindAction; {n}] = [\n");
        let body = "    BindAction::Save,\n";
        let base = format!("{}{}]\n", head(1), wrapped(body));
        let ours = format!(
            "{}{}]\n",
            head(2),
            wrapped(&format!("{body}    BindAction::Open,\n"))
        );
        let theirs = format!(
            "{}{}]\n",
            head(2),
            wrapped(&format!("{body}    BindAction::Quit,\n"))
        );
        let Resolution::Merged(out) = resolve(&base, &ours, &theirs, &opts()) else {
            panic!("両側が同じ値を書いたので衝突は出ない");
        };
        let entries = out.matches("    BindAction::").count();
        let declared: usize = out
            .split_once("[BindAction; ")
            .and_then(|(_, r)| r.split_once(']'))
            .and_then(|(n, _)| n.parse().ok())
            .expect("宣言長");
        assert_eq!(entries, 3, "要素は 3 件になる");
        assert_eq!(declared, 2, "宣言長は 2 のまま = 綺麗にマージされて壊れている");
        assert_ne!(
            entries, declared,
            "衝突ゼロでも整合しない。カウント検査テストが要る理由がこれ"
        );
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


    // ── 11. 自動判定 (マーカ無し) ──
    //
    // **「誤自動解決」の定義**をここで固定しておく。以下のどれかが起きたら
    // 誤自動解決であり、テストは落ちなければならない:
    //
    //   (a) 片側の追記が結果から消える (取りこぼし)
    //   (b) 元からあった行が消える / 順序が変わる
    //   (c) どちらの版にも無い行が出てくる (でっち上げ)
    //   (d) 出来上がりが構文として壊れている (JSON / TOML / 括弧)
    //   (e) 既存行の**書き換え**を追記として畳んだ
    //   (f) 同じキーの別の値を 2 つ並べた
    //
    // (a)〜(c) は `自動_解決したら必ず全部の追記が残り勝手な行は増えない` が
    // 乱数で 800 件回して確かめる。(d)〜(f) は種類ごとに個別のテストで固定する。

    fn auto() -> UnionOpts {
        UnionOpts {
            auto: true,
            ..UnionOpts::default()
        }
    }

    /// 自動判定で解決できたときの中身。解決しなければ panic する。
    fn auto_merged(base: &str, ours: &str, theirs: &str) -> String {
        match resolve(base, ours, theirs, &auto()) {
            Resolution::Merged(s) => s,
            Resolution::Conflict(s) => panic!("解決してほしかったのに衝突した:\n{s}"),
        }
    }

    /// 自動判定でも解決しないことの確認。
    fn auto_conflicted(base: &str, ours: &str, theirs: &str) {
        assert!(
            resolve(base, ours, theirs, &auto()).has_conflict(),
            "人間に返してほしい場面で自動解決した"
        );
    }

    const IGNORE: &str = "target/\nnode_modules/\n*.log\n";

    #[test]
    fn 自動_gitignore型の一覧はマーカ無しで両側の追記が残る() {
        let got = auto_merged(
            IGNORE,
            &format!("{IGNORE}dist/\n"),
            &format!("{IGNORE}.venv/\n"),
        );
        assert_eq!(got, format!("{IGNORE}dist/\n.venv/\n"));
    }

    #[test]
    fn 自動_中身を見て判定する_同じ拡張子でも一覧でなければ降りる() {
        // どちらも拡張子は無い / 同じでも、判定は中身だけで決まる。
        assert_eq!(detect(IGNORE), Some(ListKind::Flat));
        assert_eq!(
            detect("fn main() {\n    println!(\"hi\");\n}\n"),
            None,
            "コードは一覧ではない"
        );
        assert_eq!(
            detect("これは説明です。\nもう一行あります。\nさらに続きます。\n"),
            None,
            "散文は一覧ではない"
        );
        assert_eq!(
            detect("fn a() {}\nfn b() {}\nfn c() {}\n"),
            None,
            "短い関数が並んでいても一覧ではない"
        );
    }

    #[test]
    fn 自動_一覧でない中身は一切解決しない() {
        let base = "fn main() {\n    let a = 1;\n}\n";
        auto_conflicted(
            base,
            "fn main() {\n    let a = 1;\n    let b = 2;\n}\n",
            "fn main() {\n    let a = 1;\n    let c = 3;\n}\n",
        );
    }

    #[test]
    fn 自動_関数呼び出しの引数は一覧と見なさない() {
        // `(` で開く本体は対象外。引数の追加は本物の衝突である。
        let base = "fn f() {\n    call(\n        a,\n        b,\n        c,\n    );\n}\n";
        assert_eq!(detect(base), None);
    }

    #[test]
    fn 自動_配列リテラルの要素追加は両方残る() {
        let base = "pub const ITEMS: &[&str] = &[\n    \"alpha\",\n    \"beta\",\n    \"gamma\",\n];\n";
        let ours = base.replace("    \"beta\",\n", "    \"beta\",\n    \"ours\",\n");
        let theirs = base.replace("    \"beta\",\n", "    \"beta\",\n    \"theirs\",\n");
        let got = auto_merged(base, &ours, &theirs);
        assert!(got.contains("\"ours\","), "{got}");
        assert!(got.contains("\"theirs\","), "{got}");
        assert!(
            got.find("\"ours\"") < got.find("\"theirs\""),
            "順序は ours → theirs:\n{got}"
        );
    }

    const PKG: &str = "{\n  \"name\": \"demo\",\n  \"dependencies\": {\n    \"alpha\": \"^1.0.0\",\n    \"gamma\": \"^3.0.0\"\n  }\n}\n";

    #[test]
    fn 自動_package_json型はキーが違えば両方残り_json_として妥当() {
        let ours = PKG.replace(
            "    \"alpha\": \"^1.0.0\",\n",
            "    \"alpha\": \"^1.0.0\",\n    \"beta\": \"^2.0.0\",\n",
        );
        let theirs = PKG.replace(
            "    \"alpha\": \"^1.0.0\",\n",
            "    \"alpha\": \"^1.0.0\",\n    \"delta\": \"^4.0.0\",\n",
        );
        let got = auto_merged(PKG, &ours, &theirs);
        assert!(json_ok(&got), "構文が壊れている:\n{got}");
        for k in ["alpha", "beta", "delta", "gamma"] {
            assert!(got.contains(&format!("\"{k}\"")), "{k} が消えた:\n{got}");
        }
    }

    #[test]
    fn 自動_同じキーで値が違えば解決しない() {
        let ours = PKG.replace(
            "    \"alpha\": \"^1.0.0\",\n",
            "    \"alpha\": \"^1.0.0\",\n    \"beta\": \"^2.0.0\",\n",
        );
        let theirs = PKG.replace(
            "    \"alpha\": \"^1.0.0\",\n",
            "    \"alpha\": \"^1.0.0\",\n    \"beta\": \"^9.9.9\",\n",
        );
        auto_conflicted(PKG, &ours, &theirs);
    }

    #[test]
    fn 自動_同じキーで値も同じなら一本に畳む() {
        let add = PKG.replace(
            "    \"alpha\": \"^1.0.0\",\n",
            "    \"alpha\": \"^1.0.0\",\n    \"beta\": \"^2.0.0\",\n",
        );
        let got = auto_merged(PKG, &add, &add);
        assert_eq!(got.matches("\"beta\"").count(), 1, "二重になった:\n{got}");
        assert!(json_ok(&got));
    }

    const TOML: &str = "[dependencies]\nserde = \"1\"\nanyhow = \"1\"\n\n[dev-dependencies]\ntempfile = \"3\"\n";

    #[test]
    fn 自動_toml_のセクションはキーが違えば両方残る() {
        let ours = TOML.replace("anyhow = \"1\"\n", "anyhow = \"1\"\nregex = \"1\"\n");
        let theirs = TOML.replace("anyhow = \"1\"\n", "anyhow = \"1\"\nonce_cell = \"1\"\n");
        let got = auto_merged(TOML, &ours, &theirs);
        assert!(toml_ok(&got), "構文が壊れている:\n{got}");
        assert!(got.contains("regex = ") && got.contains("once_cell = "), "{got}");
    }

    #[test]
    fn 自動_toml_で同じキーが両側から来たら解決しない() {
        let ours = TOML.replace("anyhow = \"1\"\n", "anyhow = \"1\"\nregex = \"1\"\n");
        let theirs = TOML.replace("anyhow = \"1\"\n", "anyhow = \"1\"\nregex = \"2\"\n");
        auto_conflicted(TOML, &ours, &theirs);
    }

    const CHANGELOG: &str =
        "# Changelog\n\n## Unreleased\n\n- 既存の項目\n- もう一つ\n- 三つ目\n";

    #[test]
    fn 自動_changelog_は両方残し_重複を畳まない() {
        // 一覧は集合なので畳むが、追記帳は「同じ文面の 2 件」がありうる。
        // (両側が**まったく同じ**変更をしたときは 3-way マージの規則で
        //  1 本になるので、片方だけ重なる形で確かめる。)
        assert_eq!(detect(CHANGELOG), Some(ListKind::Journal));
        let got = auto_merged(
            CHANGELOG,
            &format!("{CHANGELOG}- 同じ文面\n- ours 固有\n"),
            &format!("{CHANGELOG}- 同じ文面\n- theirs 固有\n"),
        );
        assert_eq!(
            got.matches("- 同じ文面").count(),
            2,
            "追記帳では畳まない:\n{got}"
        );
        // 対して一覧は畳む。
        let g2 = auto_merged(
            IGNORE,
            &format!("{IGNORE}dist/\nours/\n"),
            &format!("{IGNORE}dist/\ntheirs/\n"),
        );
        assert_eq!(g2.matches("dist/").count(), 1, "一覧は畳む:\n{g2}");
    }

    #[test]
    fn 自動_changelog_で同じ見出しを両側が足したら解決しない() {
        let ours = CHANGELOG.replace("## Unreleased\n", "## 0.2.0\n\n- ours\n\n## Unreleased\n");
        let theirs = CHANGELOG.replace("## Unreleased\n", "## 0.2.0\n\n- theirs\n\n## Unreleased\n");
        auto_conflicted(CHANGELOG, &ours, &theirs);
    }

    const MODS: &str = "// 先頭のコメント\nfn helper() { }\nmod app;\nmod git;\nmod term;\n\nfn main() {}\n";

    #[test]
    fn 自動_宣言の連続ブロックだけが対象になる() {
        assert_eq!(detect(MODS), Some(ListKind::Imports));
        let ours = MODS.replace("mod git;\n", "mod git;\nmod alpha;\n");
        let theirs = MODS.replace("mod git;\n", "mod git;\nmod beta;\n");
        let got = auto_merged(MODS, &ours, &theirs);
        assert!(got.contains("mod alpha;") && got.contains("mod beta;"), "{got}");
        // ブロックの外 (関数本体) は対象外なので、そこは衝突のまま人間に返る。
        let o2 = MODS.replace("fn main() {}", "fn main() { ours() }");
        let t2 = MODS.replace("fn main() {}", "fn main() { theirs() }");
        auto_conflicted(MODS, &o2, &t2);
    }

    #[test]
    fn 自動_片側が既存行を変更したら解決しない() {
        // 書き換え × 同じ場所への追記
        auto_conflicted(
            IGNORE,
            &IGNORE.replace("*.log\n", "*.log.bak\n"),
            &format!("{IGNORE}dist/\n"),
        );
        // 削除 × 同じ場所への追記
        auto_conflicted(
            IGNORE,
            &IGNORE.replace("node_modules/\n", ""),
            &IGNORE.replace("node_modules/\n", "node_modules/\nextra/\n"),
        );
    }

    #[test]
    fn 自動_マーカがあればマーカが勝つ() {
        // マーカで囲った内側だけが対象 = 外側の追記は解決しない。
        let base = wrapped("- a\n");
        let ours = format!("{}外側から\n", wrapped("- a\n- ours\n"));
        let theirs = format!("{}外側から2\n", wrapped("- a\n- theirs\n"));
        assert!(
            resolve(&base, &ours, &theirs, &auto()).has_conflict(),
            "マーカの外は自動判定で拾わない"
        );
    }

    #[test]
    fn 自動_crlf_のファイルは_crlf_のまま返る() {
        let crlf = |s: &str| s.replace('\n', "\r\n");
        let got = auto_merged(
            &crlf(IGNORE),
            &crlf(&format!("{IGNORE}dist/\n")),
            &crlf(&format!("{IGNORE}.venv/\n")),
        );
        assert_eq!(got, crlf(&format!("{IGNORE}dist/\n.venv/\n")));
        assert!(!got.contains("\n\n"), "LF へ潰していない: {got:?}");
    }

    #[test]
    fn 自動_順序は常に_ours_から_theirs_で決定的() {
        let o = format!("{IGNORE}zzz/\n");
        let t = format!("{IGNORE}aaa/\n");
        let got = auto_merged(IGNORE, &o, &t);
        assert_eq!(got, format!("{IGNORE}zzz/\naaa/\n"), "辞書順ではなく ours→theirs");
        // 何度呼んでも同じ (`HashMap` の反復順が漏れていない)。
        for _ in 0..8 {
            assert_eq!(auto_merged(IGNORE, &o, &t), got);
        }
    }

    // ── 12. 不変条件を乱数で確かめる (誤自動解決 0 件の根拠) ──

    /// 決定的な擬似乱数。**実行ごとに変わってはいけない** (再現できないテストは
    /// 落ちたときに何も教えてくれない)。
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 11
        }
        fn pick(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    /// `sub` が `all` の部分列か (元の行が順序どおり全部残っているか)。
    fn is_subsequence(sub: &[&str], all: &[&str]) -> bool {
        let mut it = all.iter();
        sub.iter().all(|x| it.any(|y| y == x))
    }

    /// 隙間へ行を挿し込んだ版を作る。
    fn insert_at(base: &[String], at: &[(usize, String)]) -> String {
        let mut out: Vec<String> = Vec::new();
        for (i, l) in base.iter().enumerate() {
            for (g, s) in at {
                if *g == i {
                    out.push(s.clone());
                }
            }
            out.push(l.clone());
        }
        for (g, s) in at {
            if *g >= base.len() {
                out.push(s.clone());
            }
        }
        out.join("\n") + "\n"
    }

    #[test]
    fn 自動_解決したら必ず全部の追記が残り勝手な行は増えない() {
        let mut rng = Rng(0x5EED_1234);
        let mut resolved = 0usize;
        for shape in 0..4u32 {
            for _ in 0..200 {
                let n = 3 + rng.pick(6);
                let base: Vec<String> = (0..n)
                    .map(|i| match shape {
                        0 => format!("path_{i}/"),
                        1 => format!("    \"k{i}\": \"1.0\","),
                        2 => format!("key_{i} = \"1\""),
                        _ => format!("- 既存 {i}"),
                    })
                    .collect();
                let (head, tail) = match shape {
                    0 => (Vec::new(), Vec::new()),
                    1 => (
                        vec!["{".to_string(), "  \"deps\": {".to_string()],
                        vec!["    \"zz\": \"9\"".to_string(), "  }".to_string(), "}".to_string()],
                    ),
                    2 => (vec!["[dependencies]".to_string()], Vec::new()),
                    _ => (vec!["# Changelog".to_string(), String::new(), "## Unreleased".to_string(), String::new()], Vec::new()),
                };
                let full: Vec<String> = head
                    .iter()
                    .chain(base.iter())
                    .chain(tail.iter())
                    .cloned()
                    .collect();
                let gap0 = head.len();
                let mk = |rng: &mut Rng, who: &str, cnt: usize| -> Vec<(usize, String)> {
                    (0..cnt)
                        .map(|j| {
                            let g = gap0 + rng.pick(base.len() + 1);
                            let s = match shape {
                                0 => format!("{who}_{j}/"),
                                1 => format!("    \"{who}{j}\": \"2.0\","),
                                2 => format!("{who}{j} = \"2\""),
                                _ => format!("- {who} の追記 {j}"),
                            };
                            (g, s)
                        })
                        .collect()
                };
                let (no, nt) = (1 + rng.pick(3), 1 + rng.pick(3));
                let oa = mk(&mut rng, "ours", no);
                let ta = mk(&mut rng, "theirs", nt);
                let bs = full.join("\n") + "\n";
                let os = insert_at(&full, &oa);
                let ts = insert_at(&full, &ta);
                let r = resolve(&bs, &os, &ts, &auto());
                let Resolution::Merged(got) = r else { continue };
                resolved += 1;
                let got_lines: Vec<&str> = got.lines().collect();
                let base_lines: Vec<&str> = bs.lines().collect();
                // (b) 元の行が順序どおり全部残っている
                assert!(
                    is_subsequence(&base_lines, &got_lines),
                    "元の行が消えた/並び替わった:\n{got}"
                );
                // (a) 両側の追記が全部残っている
                for (_, s) in oa.iter().chain(ta.iter()) {
                    assert!(got.contains(s.as_str()), "追記が消えた {s:?}:\n{got}");
                }
                // (c) どちらの版にも無い行は出てこない
                let known: BTreeSet<&str> = bs.lines().chain(os.lines()).chain(ts.lines()).collect();
                for l in &got_lines {
                    assert!(known.contains(l), "でっち上げの行 {l:?}:\n{got}");
                }
                // (d) 構文が壊れていない
                if shape == 1 {
                    assert!(json_ok(&got), "JSON が壊れた:\n{got}");
                }
                if shape == 2 {
                    assert!(toml_ok(&got), "TOML が壊れた:\n{got}");
                }
                // 決定的
                assert_eq!(resolve(&bs, &os, &ts, &auto()).text(), got, "結果が揺れた");
            }
        }
        assert!(resolved > 400, "解決した件数が少なすぎる ({resolved}/800)");
    }

    // ── 13. 部品の単体テスト ──

    #[test]
    fn json_の受理と拒否() {
        assert!(json_ok("{\"a\": 1, \"b\": [1, 2, null]}"));
        assert!(json_ok("[]"));
        assert!(!json_ok("{\"a\": 1,}"), "末尾のカンマは JSON では不正");
        assert!(!json_ok("{\"a\": 1, \"a\": 2}"), "重複キーは壊れている扱い");
        assert!(!json_ok("{\"a\": 1"), "閉じていない");
    }

    #[test]
    fn toml_の受理と拒否() {
        assert!(toml_ok("[a]\nx = 1\ny = 2\n"));
        assert!(toml_ok("[a]\nx = 1\n\n[b]\nx = 2\n"), "別セクションなら同名でよい");
        assert!(!toml_ok("[a]\nx = 1\nx = 2\n"), "同じセクションの重複キー");
    }

    #[test]
    fn キーは二重コロンを跨がない() {
        // ここを間違えると `BindAction::Save,` の追記が全部衝突になる。
        assert_eq!(entry_key("    BindAction::Act1,", ListKind::Flat), None);
        assert_eq!(
            entry_key("    \"serde\": \"1\",", ListKind::Bracket).as_deref(),
            Some("serde")
        );
        assert_eq!(entry_key("serde = \"1\"", ListKind::Flat).as_deref(), Some("serde"));
        assert_eq!(
            entry_key("requests==2.31.0", ListKind::Flat).as_deref(),
            Some("requests")
        );
        assert_eq!(entry_key("mod app;", ListKind::Imports).as_deref(), Some("mod:app"));
        assert_eq!(entry_key("target/", ListKind::Flat), None);
        assert_eq!(entry_key("- 箇条書き", ListKind::Journal), None);
        assert_eq!(
            entry_key("## 0.2.0", ListKind::Journal).as_deref(),
            Some("#0.2.0")
        );
    }

    #[test]
    fn 自動判定のブロックは重ならず昇順() {
        for text in [IGNORE, PKG, TOML, CHANGELOG, MODS] {
            let Some(p) = detect_lines(&split_lines(text)) else {
                panic!("判定できない: {text:?}");
            };
            let mut prev = 0usize;
            for (s, e) in &p.blocks {
                assert!(*s <= *e, "空でない区間 {s}..{e}");
                assert!(*s >= prev, "重なっている {s}..{e} (前は {prev} まで)");
                prev = *e + 1;
            }
        }
    }

    // ── 14. `.gitattributes` の自動生成 ──

    /// 一覧・散文・コードが 1 つずつ入った使い捨てリポジトリ。
    fn suggest_repo(tag: &str) -> Option<PathBuf> {
        let repo = temp_repo(tag)?;
        std::fs::write(repo.join(".gitignore"), IGNORE).ok()?;
        std::fs::write(repo.join("README.md"), "# Demo\n\nこれは説明です。\n").ok()?;
        std::fs::create_dir_all(repo.join("src")).ok()?;
        std::fs::write(
            repo.join("src/main.rs"),
            "fn main() {\n    println!(\"hi\");\n}\n",
        )
        .ok()?;
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "base"]);
        Some(repo)
    }

    #[test]
    fn 提案は実在して中身が一覧のファイルだけを出す() {
        let Some(repo) = suggest_repo("suggest") else {
            println!("git が無い環境なのでスキップ");
            return;
        };
        let got = suggest_attributes(&repo);
        let pats: Vec<&str> = got.iter().map(|a| a.pattern.as_str()).collect();
        assert!(pats.contains(&".gitignore"), "{pats:?}");
        assert!(!pats.iter().any(|p| p.contains("README")), "散文は出さない: {pats:?}");
        assert!(!pats.iter().any(|p| p.contains("main.rs")), "コードは出さない: {pats:?}");
        assert!(!pats.iter().any(|p| *p == "*.rs"), "存在しない意味のパターンを並べない: {pats:?}");
        for a in &got {
            assert_eq!(a.driver, AUTO_DRIVER);
            assert!(!a.why.is_empty(), "根拠を書く");
            assert!(a.files >= 1, "実在するファイルの数");
        }
        // 並びは決定的。
        assert_eq!(
            suggest_attributes(&repo)
                .iter()
                .map(|a| a.pattern.clone())
                .collect::<Vec<_>>(),
            got.iter().map(|a| a.pattern.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn 導入は既存の_merge_指定を上書きしない() {
        let Some(repo) = suggest_repo("keep-attr") else {
            println!("git が無い環境なのでスキップ");
            return;
        };
        let keep = ".gitignore merge=ours\n";
        std::fs::write(repo.join(".gitattributes"), keep).expect("write");
        let rep = install_with(&repo, &repo.join("fake-zai"), None).expect("導入");
        assert!(
            rep.skipped.iter().any(|a| a.pattern == ".gitignore"),
            "既存の指定があるものは見送る: {:?}",
            rep.skipped
        );
        assert!(
            !rep.added.iter().any(|a| a.pattern == ".gitignore"),
            "上書きしていない"
        );
        let after = std::fs::read_to_string(repo.join(".gitattributes")).expect("読める");
        assert!(after.starts_with(keep), "既存の行がそのまま先頭に残る:\n{after}");
        uninstall(&repo).expect("解除");
        assert_eq!(
            std::fs::read_to_string(repo.join(".gitattributes")).expect("読める"),
            keep,
            "解除したら元どおり"
        );
    }

    #[test]
    fn 自動生成した_gitattributes_は冪等() {
        let Some(repo) = suggest_repo("idem") else {
            println!("git が無い環境なのでスキップ");
            return;
        };
        let mut prev = String::new();
        for i in 0..3 {
            let rep = install_with(&repo, &repo.join("fake-zai"), None).expect("導入");
            assert_eq!(rep.drivers, DRIVERS.len());
            let now = std::fs::read_to_string(repo.join(".gitattributes")).expect("読める");
            if i > 0 {
                assert_eq!(now, prev, "何度書いても同じ中身");
            }
            assert_eq!(now.matches(ATTR_BEGIN_KEY).count(), 1);
            prev = now;
        }
        assert!(prev.contains(&format!("merge={AUTO_DRIVER}")), "{prev}");
        uninstall(&repo).expect("解除");
        assert!(!repo.join(".gitattributes").exists(), "こちらが作ったものは消す");
        assert!(!is_installed(&repo));
    }

    // ── 15. 統合: マーカ無しのリポジトリを本物の git にマージさせる ──

    /// 自分自身をドライバとして登録するときのコマンド行。
    fn helper_driver(exe: &Path, flags: &str) -> String {
        format!(
            "ZV_UNION_FLAGS=\"{flags}\" ZV_UNION_O=\"%O\" ZV_UNION_A=\"%A\" ZV_UNION_B=\"%B\" ZV_UNION_L=\"%L\" ZV_UNION_P=\"%P\" {} --exact {} --quiet",
            sh_quote(exe),
            helper_test_name()
        )
    }

    #[test]
    fn 本物の_git_マージでマーカ無しの一覧が自動解決される() {
        let Some(repo) = temp_repo("auto-merge") else {
            println!("git が無い環境なのでスキップ");
            return;
        };
        let exe = std::env::current_exe().expect("テストバイナリの場所");
        git(&repo, &["config", "--local", &format!("merge.{AUTO_DRIVER}.name"), DRIVER_DESC]);
        git(
            &repo,
            &[
                "config",
                "--local",
                &format!("merge.{AUTO_DRIVER}.driver"),
                &helper_driver(&exe, "--auto"),
            ],
        );
        std::fs::write(
            repo.join(".gitattributes"),
            format!(".gitignore merge={AUTO_DRIVER}\nprose.md merge={AUTO_DRIVER}\n"),
        )
        .expect("write");
        std::fs::write(repo.join(".gitignore"), IGNORE).expect("write");
        let prose = "一行目です。\n二行目です。\n三行目です。\n";
        std::fs::write(repo.join("prose.md"), prose).expect("write");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "base"]);

        git(&repo, &["checkout", "-q", "-b", "featA"]);
        std::fs::write(repo.join(".gitignore"), format!("{IGNORE}from-a/\n")).expect("write");
        std::fs::write(repo.join("prose.md"), format!("{prose}A の段落です。\n")).expect("write");
        git(&repo, &["commit", "-qam", "A"]);
        git(&repo, &["checkout", "-q", "main"]);
        git(&repo, &["checkout", "-q", "-b", "featB"]);
        std::fs::write(repo.join(".gitignore"), format!("{IGNORE}from-b/\n")).expect("write");
        std::fs::write(repo.join("prose.md"), format!("{prose}B の段落です。\n")).expect("write");
        git(&repo, &["commit", "-qam", "B"]);

        let out = git(&repo, &["merge", "--no-edit", "featA"]);
        let ign = std::fs::read_to_string(repo.join(".gitignore")).expect("読める");
        assert_eq!(
            ign,
            format!("{IGNORE}from-b/\nfrom-a/\n"),
            "マーカ無しの一覧が自動解決されていない。git の出力:\n{out}"
        );
        // 散文は自動判定の対象外 = **素の git と同じ結果** (衝突が残る)。
        let pr = std::fs::read_to_string(repo.join("prose.md")).expect("読める");
        assert!(
            pr.contains("<<<<<<<") && pr.contains(">>>>>>>"),
            "散文まで勝手に混ぜてはいけない:\n{pr}"
        );
    }

    #[test]
    fn 自動判定モードは自分の衝突マーカを一度も書かない() {
        // 解決しきれない場面では `%A` を書かずに git 本体へ委譲する。
        // ラベルが git の既定 (`ours`/`base`/`theirs`) になることで確かめる。
        let Some(dir) = temp_repo("delegate") else {
            println!("git が無い環境なのでスキップ");
            return;
        };
        let ours_txt = PKG.replace(
            "    \"alpha\": \"^1.0.0\",\n",
            "    \"alpha\": \"^1.0.0\",\n    \"beta\": \"^2.0.0\",\n",
        );
        let theirs_txt = PKG.replace(
            "    \"alpha\": \"^1.0.0\",\n",
            "    \"alpha\": \"^1.0.0\",\n    \"beta\": \"^9.9.9\",\n",
        );
        let (o, a, b) = (dir.join("o"), dir.join("a"), dir.join("b"));
        std::fs::write(&o, PKG).expect("write");
        std::fs::write(&a, &ours_txt).expect("write");
        std::fs::write(&b, &theirs_txt).expect("write");
        let argv: Vec<String> = vec![
            "--auto".into(),
            o.to_string_lossy().into_owned(),
            a.to_string_lossy().into_owned(),
            b.to_string_lossy().into_owned(),
            "7".into(),
            "package.json".into(),
        ];
        let code = cli_main(&argv);
        assert_ne!(code, 0, "衝突は衝突として返す");
        let got = std::fs::read_to_string(&a).expect("読める");
        // 素の git へ丸投げした結果と 1 バイトも変わらないこと。
        let (o2, a2, b2) = (dir.join("o2"), dir.join("a2"), dir.join("b2"));
        std::fs::write(&o2, PKG).expect("write");
        std::fs::write(&a2, &ours_txt).expect("write");
        std::fs::write(&b2, &theirs_txt).expect("write");
        delegate_to_git(&o2, &a2, &b2, 7);
        assert_eq!(
            got,
            std::fs::read_to_string(&a2).expect("読める"),
            "自動判定モードで自前の衝突マーカを書いている"
        );
    }

    /// 共有ファイルへ 1 行も足していないこと (この機能の存在意義そのもの)。
    #[test]
    fn 共有ファイルを触らずに繋がっている() {
        let reg = include_str!("features/union.rs").replace("\r\n", "\n");
        assert!(reg.contains("#[path = \"../union.rs\"]"), "実体の引き込みが無い");
        assert!(reg.contains("pub use imp::{cli_main, FEATURE};"), "再エクスポートが無い");
    }
}

