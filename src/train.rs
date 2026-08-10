//! 🚃 マージトレイン — 並列エージェントの成果を **順番に・衝突ゼロで** 統合する。
//!
//! ## なぜ要るのか
//!
//! 並列で N 体のエージェントを走らせて稼いだ時間は、**統合で払い戻される**。
//! このリポジトリには今までマージキューも自動リベースも無く、`git merge` を
//! 撃つ場所は `race.rs` の `adopt_racer` 1 箇所だけで、失敗したら
//! `merge --abort` して人間に返して終わりだった。`conflict.rs` の衝突レーダーは
//! 「衝突しそうだ」と**見せる**ところで止まっていて (`RadarAction` は
//! `Open` と `Close` の 2 つしかない)、見つけた後に何ができるかが空白だった。
//!
//! ここはその空白を埋める。姉妹機能との住み分け:
//!
//! | モジュール | 役割 |
//! |---|---|
//! | `lease.rs` | 同じファイルを 2 人に触らせない (**起こさない**) |
//! | `conflict.rs` | 近い行の衝突を早く見せる (**見せる**) |
//! | `semconf.rs` | ファイルは違うのに噛み合わない変更を見せる |
//! | `train.rs` | 見つけた後に **順番を決めて実際に統合する** |
//!
//! ## 設計
//!
//! 1. **順序決定は純関数** ([`plan_order`])。他と重なりが少ないものを先に流す。
//!    先に入ったものが後続のリベース基準になるので、重なりの大きいものを
//!    最後へ回すと人手が要る回数が最小になる。同点は**ブランチ名の辞書順**で
//!    割り、`HashMap` / `HashSet` の反復順は 1 バイトも出力へ漏らさない
//!    (`Vec` と `BTreeMap` / `BTreeSet` だけで組む)。
//! 2. **実行前に必ず乾式検査** ([`dry_run`])。`git merge-tree --write-tree` を
//!    順に当てて「この順序なら衝突する」を**参照を 1 つも動かす前に**言う。
//!    使えない git (2.38 未満) では順序だけへ綺麗に降格する。
//! 3. **fail-closed**。失敗したら即 `git rebase --abort` して止め、控えておいた
//!    OID へ**全部戻す**。`-X ours` のような強行はしない。どのブランチの・
//!    どのファイルの・どの行で・誰と衝突したかを構造化して返す。
//! 4. **git を UI スレッドで待たない**。走査も実行も裏のスレッドへ逃がし、
//!    描画には**いま手元にある値**を返す (古くてよい。数秒固まるのは許されない)。
//!
//! ## 担保できないもの (正直に書く)
//!
//! * 乾式検査は**マージで近似**する。rebase はコミットを 1 つずつ当て直すので、
//!   「途中のコミットだけが衝突して、最終形は綺麗に混ざる」ケースを乾式は
//!   見落とす。そこは本番で fail-closed に止まって全部戻る (テスト済み)。
//! * **他のワークツリーが握っているブランチは動かさない。** 作業中の
//!   エージェントの足元で履歴を書き換えるのは事故そのものなので、対象から
//!   外して画面に理由を出す (CLAUDE.md の「統合したワークツリーは即座に消す」に
//!   従っていれば、終わったブランチは自然に空く)。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::conflict::{FileEdit, Span};
use crate::i18n::{tr, trf};
use crate::panels::space;
use crate::worktree::git_out;

// ═══════════════════════════════════════════════════════════════════════
//  1. 上限と定数
// ═══════════════════════════════════════════════════════════════════════

/// 走査の最短間隔。遅いリポジトリでは [`crate::git::scan_interval`] が
/// 直近の所要時間の 4 倍まで自動で空ける (git を常時走らせない)。
const SCAN_BASE: Duration = Duration::from_secs(4);

/// 1 度に列車へ載せるブランチの上限。総当たりの重なり計算が O(N²) なので
/// 歯止めを置く。超えたぶんは**画面に件数を出す** (黙って切らない)。
pub const MAX_BRANCHES: usize = 24;

/// 衝突した行として報告する上限。
pub const MAX_CONFLICT_LINES: usize = 40;

/// 重なりファイルをホバーに出す上限。
const MAX_SHOWN_FILES: usize = 8;

/// 乾式検査で作る作業コミットのメッセージ。**どの参照からも指されない**ので
/// `git gc` が普通に回収する。
const DRY_COMMIT_MSG: &str = "zaivern train dry-run (unreferenced)";

/// 乾式検査の作業コミットに使う身元。リポジトリの設定を汚さないよう
/// `-c` でその 1 回だけ渡す (`user.email` 未設定の環境でも乾式は通る)。
const DRY_IDENT: [&str; 4] = [
    "-c",
    "user.name=zaivern-train",
    "-c",
    "user.email=train@zaivern.invalid",
];

// ═══════════════════════════════════════════════════════════════════════
//  2. 入力 — 各ブランチが触った範囲
// ═══════════════════════════════════════════════════════════════════════

/// 1 ブランチが触った範囲。行範囲は**任意** (無ければファイル単位まで降格)。
///
/// `BTreeSet` / `BTreeMap` しか使わないのは、順序決定の出力へハッシュの
/// 反復順を漏らさないため。同じ入力からは必ず同じ計画が出る。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BranchTouch {
    pub branch: String,
    /// 触ったファイル (正規化済みのリポジトリ相対パス)。
    pub files: BTreeSet<String>,
    /// ベース側の変更行範囲。キーは [`BranchTouch::files`] の要素。
    /// **空でよい** — 行が分からないときはファイル単位で判定する。
    pub spans: BTreeMap<String, Vec<Span>>,
}

/// [`crate::conflict::FileEdit`] の列から作る。差分の読み取りは
/// `conflict.rs` が既に持っているので**再実装しない**。
pub fn touch_from_edits(branch: &str, edits: &[FileEdit]) -> BranchTouch {
    let mut t = BranchTouch {
        branch: branch.to_string(),
        ..Default::default()
    };
    for e in edits {
        if e.path.is_empty() {
            continue;
        }
        t.files.insert(e.path.clone());
        if !e.spans.is_empty() {
            t.spans
                .entry(e.path.clone())
                .or_default()
                .extend(e.spans.iter().copied());
        }
    }
    for v in t.spans.values_mut() {
        v.sort();
        v.dedup();
    }
    t
}

/// リポジトリから各ブランチの触った範囲を集める。
///
/// **裏のスレッドから呼ぶこと。** ブランチ 1 本あたり git を 2 回起動する。
pub fn touches_from_repo(repo: &Path, onto: &str, branches: &[String]) -> Vec<BranchTouch> {
    let mut out = Vec::new();
    for b in branches.iter().take(MAX_BRANCHES) {
        let base = git_out(repo, &["merge-base", onto, b])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let edits = if base.is_empty() {
            Vec::new()
        } else {
            git_out(
                repo,
                &[
                    "diff",
                    "--unified=0",
                    "--no-color",
                    "--no-ext-diff",
                    "--find-renames",
                    &base,
                    b,
                ],
            )
            .map(|d| crate::conflict::edits_from_diff(&d))
            .unwrap_or_default()
        };
        out.push(touch_from_edits(b, &edits));
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════
//  3. 重なりの強さ
// ═══════════════════════════════════════════════════════════════════════

/// 2 本のブランチの重なり。順序は「弱い → 強い」で、[`Ord`] をそのまま
/// 「最悪値を取る」に使う。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Risk {
    /// 触ったファイルが 1 つも被っていない。
    #[default]
    Clear,
    /// 同じファイルを触るが、行域は重なっていない (git は大抵綺麗に混ぜる)。
    File,
    /// 同じ行域が重なる。**まず確実に人手が要る。**
    Line,
}

impl Risk {
    /// 画面とレポートに出す見出し (**日本語の原文**。表示時に [`tr`] を通す)。
    pub fn label(self) -> &'static str {
        match self {
            Risk::Clear => "衝突なし",
            Risk::File => "同じファイル (行は別)",
            Risk::Line => "同じ行 — 衝突しそう",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Risk::Clear => "✅",
            Risk::File => "△",
            Risk::Line => "⚠",
        }
    }
}

/// 2 本のブランチの重なり (純関数)。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Pair {
    pub risk: Risk,
    /// 両方が触ったファイル (辞書順)。
    pub files: Vec<String>,
}

/// 2 本のブランチがどれだけ重なるかを見る (純関数)。
///
/// 行範囲が**片方でも無い**ファイルは [`Risk::File`] 止まりにする —
/// 「行は別だ」と言い切れる根拠が無いのに [`Risk::Clear`] にすると
/// 見落としになり、[`Risk::Line`] にすると鳴りすぎる。
pub fn risk_between(a: &BranchTouch, b: &BranchTouch) -> Pair {
    let mut out = Pair::default();
    for f in a.files.intersection(&b.files) {
        out.files.push(f.clone());
        let worst = match (a.spans.get(f), b.spans.get(f)) {
            (Some(sa), Some(sb)) if !sa.is_empty() && !sb.is_empty() => {
                if sa.iter().any(|x| sb.iter().any(|y| x.overlaps(*y))) {
                    Risk::Line
                } else {
                    Risk::File
                }
            }
            _ => Risk::File,
        };
        out.risk = out.risk.max(worst);
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════
//  4. 順序決定 — 純関数
// ═══════════════════════════════════════════════════════════════════════

/// 統合 1 段。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TrainStep {
    /// 何番目に流すか (0 始まり)。
    pub position: usize,
    pub branch: String,
    /// このブランチが触ったファイル数。
    pub files: usize,
    /// **先に入る**ブランチとの重なりだけを載せる (後ろとの重なりは
    /// その段で判定するので、ここに混ぜると二重に数えることになる)。
    pub overlaps: Vec<Overlap>,
    /// この段で予想される結果 = [`TrainStep::overlaps`] の最悪値。
    pub expect: Risk,
}

/// 先に入るブランチ 1 本との重なり。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Overlap {
    pub with: String,
    pub risk: Risk,
    pub files: Vec<String>,
}

/// 統合の計画。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TrainPlan {
    pub steps: Vec<TrainStep>,
    /// 行域まで重なった組の数 (**順序をどう変えても消えない**人手の下限)。
    pub line_pairs: usize,
    /// [`MAX_BRANCHES`] を超えて載せられなかった本数。0 でなければ画面に出す。
    pub dropped: usize,
}

impl TrainPlan {
    /// 統合順のブランチ名。
    pub fn order(&self) -> Vec<String> {
        self.steps.iter().map(|s| s.branch.clone()).collect()
    }
}

/// `(小さい方, 大きい方)` に正規化した鍵で引く。
fn pair_of<'a>(m: &'a BTreeMap<(String, String), Pair>, a: &str, b: &str) -> Option<&'a Pair> {
    let key = if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    };
    m.get(&key)
}

/// **順序決定 (純関数)。** 他と重なりが少ないものを先に流す。
///
/// 先に入ったものが後続のリベース基準になるので、重なりの大きいものを最後へ
/// 回すと「人手が要る回数」が最小になる。各段で**残っている相手**との重なりを
/// 数え直す貪欲法で、優先度は
/// `(行で重なる相手の数, ファイルで重なる相手の数, 重なりファイルの総数,
///   自分が触ったファイル数, ブランチ名)` の辞書順。
///
/// 4 番目 (自分の触ったファイル数) が要るのは、残りの相手が同数になったときに
/// **足跡の広いほうを後ろへ回す**ため。これが無いと、2 ファイルを触る `hub` と
/// 1 ファイルの `tb` が「残り 1 本と重なる」で並び、名前だけで決まってしまう。
/// 最後の項が名前なので**同点でも必ず一意に決まる** (`HashMap` の反復順は
/// 出力へ 1 バイトも漏れない)。
pub fn plan_order(touches: &[BranchTouch]) -> TrainPlan {
    // 入力の並びを出力へ漏らさない。同名は先勝ち。
    let mut by_name: BTreeMap<String, &BranchTouch> = BTreeMap::new();
    let mut dropped = 0usize;
    for t in touches {
        if t.branch.is_empty() {
            continue;
        }
        if by_name.len() >= MAX_BRANCHES && !by_name.contains_key(&t.branch) {
            dropped += 1;
            continue;
        }
        by_name.entry(t.branch.clone()).or_insert(t);
    }
    let names: Vec<String> = by_name.keys().cloned().collect();

    // 総当たりの重なり。重ならない組は持たない (引けないことが「重ならない」)。
    let mut pairs: BTreeMap<(String, String), Pair> = BTreeMap::new();
    for (i, a) in names.iter().enumerate() {
        for b in names.iter().skip(i + 1) {
            let p = risk_between(by_name[a], by_name[b]);
            if p.risk != Risk::Clear {
                pairs.insert((a.clone(), b.clone()), p);
            }
        }
    }
    let line_pairs = pairs.values().filter(|p| p.risk == Risk::Line).count();

    // 貪欲に「残りとの重なりが最小」を選ぶ。
    let mut remaining: Vec<String> = names.clone();
    let mut order: Vec<String> = Vec::with_capacity(names.len());
    while !remaining.is_empty() {
        let mut best: Option<(usize, usize, usize, usize, String)> = None;
        for c in &remaining {
            let mut line_peers = 0usize;
            let mut file_peers = 0usize;
            let mut shared = 0usize;
            for o in &remaining {
                if o == c {
                    continue;
                }
                let Some(p) = pair_of(&pairs, c, o) else {
                    continue;
                };
                match p.risk {
                    Risk::Line => line_peers += 1,
                    Risk::File => file_peers += 1,
                    Risk::Clear => {}
                }
                shared += p.files.len();
            }
            let key = (
                line_peers,
                file_peers,
                shared,
                by_name[c].files.len(),
                c.clone(),
            );
            if best.as_ref().is_none_or(|b| key < *b) {
                best = Some(key);
            }
        }
        let pick = best.expect("remaining が空でないなら必ず選ばれる").4;
        remaining.retain(|x| x != &pick);
        order.push(pick);
    }

    // 各段で「先に入るもの」との重なりを畳む。
    let mut steps = Vec::with_capacity(order.len());
    for (i, name) in order.iter().enumerate() {
        let mut overlaps = Vec::new();
        let mut expect = Risk::Clear;
        for prev in &order[..i] {
            let Some(p) = pair_of(&pairs, name, prev) else {
                continue;
            };
            expect = expect.max(p.risk);
            overlaps.push(Overlap {
                with: prev.clone(),
                risk: p.risk,
                files: p.files.clone(),
            });
        }
        steps.push(TrainStep {
            position: i,
            branch: name.clone(),
            files: by_name[name].files.len(),
            overlaps,
            expect,
        });
    }
    TrainPlan {
        steps,
        line_pairs,
        dropped,
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  5. 乾式検査 — 参照を 1 つも動かさずに結果を予告する
// ═══════════════════════════════════════════════════════════════════════

/// 乾式検査 1 段。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DryStep {
    pub branch: String,
    /// 衝突すると言われたファイル。空なら綺麗に入る。
    pub conflicts: Vec<String>,
}

/// 乾式検査の結果。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DryResult {
    /// `git merge-tree --write-tree` (git 2.38+) が使えたか。
    /// false なら順序だけへ降格していて、[`DryResult::steps`] は空。
    pub available: bool,
    /// 統合順に並んだ予想。**最初に衝突した段で打ち切る**
    /// (そこから先は順序が変わるので、予想しても嘘になる)。
    pub steps: Vec<DryStep>,
    /// 降格・打ち切りの理由。**必ず画面に出す** (無音で切らない)。
    pub note: Option<String>,
}

impl DryResult {
    /// 衝突すると言われた最初の段。
    pub fn first_conflict(&self) -> Option<&DryStep> {
        self.steps.iter().find(|s| !s.conflicts.is_empty())
    }
}

/// `git merge-tree --write-tree` の生の stdout を取る。
///
/// **衝突したときも終了コードは 1** なので [`git_out`] は `Err` を返すが、
/// 判定に要る stdout はそのメッセージの中にいる (`git_out` は stderr が
/// 空なら stdout を載せる)。剥がす接頭辞は `git_out` 自身が組み立てる
/// `"git <サブコマンド>: "` なので、**git の英語文言には依存しない**。
fn merge_tree_raw(repo: &Path, a: &str, b: &str) -> Option<String> {
    let args = crate::conflict::merge_tree_argv(a, b);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    match git_out(repo, &argv) {
        Ok(s) => Some(s),
        Err(e) => e.strip_prefix("git merge-tree: ").map(str::to_string),
    }
}

/// `merge-tree` の出力の先頭 (= マージ結果のツリー OID)。
fn first_oid(raw: &str) -> Option<String> {
    let head = raw.split('\0').next()?.trim();
    (head.len() >= 40 && head.chars().all(|c| c.is_ascii_hexdigit())).then(|| head.to_string())
}

/// **実行前の乾式検査。** 参照を 1 つも動かさない。
///
/// 段ごとにマージ結果のツリーから `commit-tree` で作業コミットを作り、
/// 次の段の相手にする。こうしないと 2 段目以降が「まだ入っていない状態」に
/// 対する判定になり、画面の予想と本番がずれる。作業コミットはどの参照からも
/// 指されないので `git gc` が回収する。
pub fn dry_run(repo: &Path, onto: &str, order: &[String]) -> DryResult {
    let version = git_out(repo, &["--version"]).unwrap_or_default();
    if !crate::conflict::supports_merge_tree(&version) {
        return DryResult {
            available: false,
            steps: Vec::new(),
            note: Some(trf(
                "{v} には merge-tree --write-tree がありません。順序だけを出しています。",
                &[("v", version.trim().to_string())],
            )),
        };
    }
    let Ok(mut head) = rev(repo, onto) else {
        return DryResult {
            available: false,
            steps: Vec::new(),
            note: Some(trf(
                "統合先 {b} が見つかりません",
                &[("b", onto.to_string())],
            )),
        };
    };
    let mut steps = Vec::new();
    let mut note = None;
    for b in order {
        let Some(raw) = merge_tree_raw(repo, &head, b) else {
            note = Some(trf("{b} の乾式検査ができませんでした", &[("b", b.clone())]));
            break;
        };
        let Some(tree) = first_oid(&raw) else {
            note = Some(trf(
                "{b} の乾式検査ができませんでした: {m}",
                &[("b", b.clone()), ("m", raw.lines().next().unwrap_or("").into())],
            ));
            break;
        };
        let conflicts = crate::conflict::parse_merge_tree(&raw).unwrap_or_default();
        let clean = conflicts.is_empty();
        steps.push(DryStep {
            branch: b.clone(),
            conflicts,
        });
        if !clean {
            break;
        }
        let mut argv: Vec<&str> = DRY_IDENT.to_vec();
        argv.extend_from_slice(&["commit-tree", &tree, "-p", &head, "-p", b, "-m", DRY_COMMIT_MSG]);
        match git_out(repo, &argv) {
            Ok(c) if !c.trim().is_empty() => head = c.trim().to_string(),
            _ => {
                note = Some(trf(
                    "{b} まで確認しました (それ以降は判定できていません)",
                    &[("b", b.clone())],
                ));
                break;
            }
        }
    }
    DryResult {
        available: true,
        steps,
        note,
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  6. 実行 — fail-closed
// ═══════════════════════════════════════════════════════════════════════

/// 統合の依頼。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrainRequest {
    pub repo: PathBuf,
    /// 統合先ブランチ。
    pub onto: String,
    /// 載せるブランチ (順序は [`plan_order`] が決め直す)。
    pub branches: Vec<String>,
    /// true なら乾式検査までで止める (参照を 1 つも動かさない)。
    pub dry_run: bool,
}

/// 止まった理由。**どのブランチの・どのファイルの・どの行で・誰と** を持つ。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TrainStop {
    pub branch: String,
    /// 衝突したファイル。
    pub files: Vec<String>,
    /// 衝突した位置 (`パス:行`)。乾式で止めたときは空。
    pub lines: Vec<String>,
    /// 相手 — 既に入っているブランチのうち、同じファイルを触ったもの。
    pub against: Vec<String>,
    /// git が返した原文 (英語)。判定には使わない。
    pub detail: String,
    /// **参照を動かす前**に乾式検査で止めたか。
    pub predicted: bool,
}

/// 統合の結果。
#[derive(Clone, Debug, Default, Serialize)]
pub struct TrainReport {
    pub onto: String,
    pub plan: TrainPlan,
    pub dry: DryResult,
    /// 実際に統合できたブランチ (統合順)。
    pub merged: Vec<String>,
    /// 止まったなら理由。`None` なら全部入った。
    pub stop: Option<TrainStop>,
    /// 止まったあと、開始時の状態へ戻せたか。
    pub restored: bool,
    pub log: Vec<String>,
    pub took_ms: u128,
}

impl TrainReport {
    pub fn ok(&self) -> bool {
        self.stop.is_none()
    }
}

/// `<ref>^{commit}` を解決する。無ければ日本語のエラー。
fn rev(repo: &Path, r: &str) -> Result<String, String> {
    let spec = format!("{r}^{{commit}}");
    match git_out(repo, &["rev-parse", "--verify", "--quiet", &spec]) {
        Ok(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => Err(trf("{r} が見つかりません", &[("r", r.to_string())])),
    }
}

/// 統合の候補。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Candidates {
    /// 動かせるブランチ (統合先より先に進んでいるもの)。
    pub free: Vec<String>,
    /// 別のワークツリーが握っていて**動かしてはいけない**ブランチ。
    pub held: Vec<(String, PathBuf)>,
}

/// 統合先を推測する。`origin/HEAD` → 現在のブランチ の順で降りる。
/// **ブランチ名は 1 つもハードコードしない。**
pub fn default_onto(repo: &Path) -> String {
    let current = git_out(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let head = git_out(
        repo,
        &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"],
    )
    .unwrap_or_default();
    if let Some((_, b)) = head.trim().split_once('/') {
        if !b.is_empty() && rev(repo, b).is_ok() {
            return b.to_string();
        }
    }
    current
}

/// 統合できるブランチを集める。**裏のスレッドから呼ぶこと。**
pub fn candidates(repo: &Path, onto: &str) -> Candidates {
    let mut out = Candidates::default();
    let porcelain = git_out(repo, &["worktree", "list", "--porcelain"]).unwrap_or_default();
    let holders = crate::git::worktree_holders(&porcelain, repo);
    let all = git_out(
        repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )
    .unwrap_or_default();
    for b in all.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if b == onto {
            continue;
        }
        let ahead = git_out(repo, &["rev-list", "--count", &format!("{onto}..{b}")])
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if ahead == 0 {
            continue;
        }
        match holders.iter().find(|(n, _)| n == b) {
            Some((_, d)) => out.held.push((b.to_string(), d.clone())),
            None => out.free.push(b.to_string()),
        }
    }
    out
}

/// 衝突しているファイルと行 (`パス:行`) を取る。**言語に依存しない**
/// — `git` の英語文言ではなく、作業ツリーに残ったマーカー行を数える。
fn conflict_details(top: &Path) -> (Vec<String>, Vec<String>) {
    let files: Vec<String> = git_out(top, &["diff", "--name-only", "--diff-filter=U"])
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(crate::conflict::norm_path)
        .collect();
    let mut lines = Vec::new();
    for f in &files {
        if lines.len() >= MAX_CONFLICT_LINES {
            break;
        }
        let Ok(text) = std::fs::read_to_string(top.join(f)) else {
            continue;
        };
        for (i, l) in text.lines().enumerate() {
            if l.starts_with("<<<<<<<") {
                lines.push(format!("{f}:{}", i + 1));
                if lines.len() >= MAX_CONFLICT_LINES {
                    break;
                }
            }
        }
    }
    (files, lines)
}

/// 開始時の状態へ**全部戻す**。戻し切れたら `true`。
///
/// 手順が `--abort` だけでは足りないのは、既に fast-forward した統合先が
/// 前へ進んだままになるため。控えた OID を `update-ref` で書き戻す前に
/// **必ず detach する** — 現在チェックアウト中のブランチは `update-ref` で
/// 動かすと index と作業ツリーが食い違う。
fn restore(
    top: &Path,
    saved: &[(String, String)],
    head_ref: &Option<String>,
    head_oid: &str,
    log: &mut Vec<String>,
) -> bool {
    let _ = git_out(top, &["rebase", "--abort"]);
    let _ = git_out(top, &["merge", "--abort"]);
    let mut ok = git_out(top, &["checkout", "--force", "--detach", head_oid]).is_ok();
    for (name, oid) in saved {
        if git_out(top, &["update-ref", &format!("refs/heads/{name}"), oid]).is_err() {
            ok = false;
        }
    }
    if let Some(r) = head_ref {
        if git_out(top, &["checkout", "--force", r]).is_err() {
            ok = false;
        }
    }
    if git_out(top, &["reset", "--hard", head_oid]).is_err() {
        ok = false;
    }
    log.push(if ok {
        tr("開始時の状態へ戻しました (ブランチも統合先も 1 つも動いていません)")
    } else {
        tr("⚠ 開始時の状態へ戻し切れませんでした。git status を確認してください")
    });
    ok
}

/// **マージトレインの実行。** 失敗したら即止めて全部戻す (fail-closed)。
///
/// **裏のスレッドから呼ぶこと。** git を何度も起動するので、UI スレッドから
/// 呼ぶと数秒〜数十秒フレームが止まる。
pub fn run_train(req: &TrainRequest) -> Result<TrainReport, String> {
    let t0 = Instant::now();
    let top = crate::worktree::repo_root(&req.repo)?;
    let mut log: Vec<String> = Vec::new();

    // ① 作業ツリーが汚れていたら始めない。追跡外のファイルは rebase を
    //    妨げないので見ない (見ると、このリポジトリでは常に始められなくなる)。
    let dirty = git_out(&top, &["status", "--porcelain", "--untracked-files=no"])?;
    if !dirty.trim().is_empty() {
        return Err(tr(
            "作業ツリーに未コミットの変更があります。コミットしてから始めてください。",
        ));
    }

    // ② 参照の実在を先に確かめる (途中で気付くと戻す手間が増える)。
    let onto_oid = rev(&top, &req.onto)?;
    let mut branches: Vec<String> = Vec::new();
    for b in &req.branches {
        if b == &req.onto || branches.contains(b) {
            continue;
        }
        rev(&top, b)?;
        branches.push(b.clone());
    }
    if branches.is_empty() {
        return Err(tr("統合するブランチがありません。"));
    }

    // ③ 他のワークツリーが握っているブランチは動かさない (作業中の
    //    エージェントの足元で履歴を書き換えない)。
    let porcelain = git_out(&top, &["worktree", "list", "--porcelain"]).unwrap_or_default();
    let held: Vec<String> = crate::git::worktree_holders(&porcelain, &top)
        .into_iter()
        .filter(|(n, _)| n == &req.onto || branches.contains(n))
        .map(|(n, d)| format!("{n} ({})", d.display()))
        .collect();
    if !held.is_empty() {
        return Err(trf(
            "別のワークツリーが使用中のブランチは動かせません: {list}",
            &[("list", held.join(", "))],
        ));
    }

    // ④ 順序を決める (純関数) → 乾式検査。
    let touches = touches_from_repo(&top, &req.onto, &branches);
    let plan = plan_order(&touches);
    let order = plan.order();
    let dry = dry_run(&top, &req.onto, &order);

    let mut report = TrainReport {
        onto: req.onto.clone(),
        plan,
        dry,
        merged: Vec::new(),
        stop: None,
        restored: true,
        log: Vec::new(),
        took_ms: 0,
    };

    // 乾式で衝突が出たら、**参照を 1 つも動かす前に**止める。
    if let Some(bad) = report.dry.first_conflict().cloned() {
        let against = touchers(&touches, &bad.conflicts, &order, &bad.branch);
        log.push(trf(
            "乾式検査: {b} がこの順序では衝突します",
            &[("b", bad.branch.clone())],
        ));
        report.stop = Some(TrainStop {
            branch: bad.branch,
            files: bad.conflicts,
            lines: Vec::new(),
            against,
            detail: String::new(),
            predicted: true,
        });
        report.log = log;
        report.took_ms = t0.elapsed().as_millis();
        return Ok(report);
    }
    if req.dry_run {
        log.push(tr("乾式検査だけを行いました (参照は 1 つも動いていません)"));
        report.log = log;
        report.took_ms = t0.elapsed().as_millis();
        return Ok(report);
    }

    // ⑤ 開始時の状態を控える。**戻せることが fail-closed の条件。**
    let head_oid = git_out(&top, &["rev-parse", "HEAD"])?.trim().to_string();
    let head_ref = git_out(&top, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let mut saved: Vec<(String, String)> = vec![(req.onto.clone(), onto_oid)];
    for b in &branches {
        saved.push((b.clone(), rev(&top, b)?));
    }

    // ⑥ 1 本ずつ rebase → fast-forward。
    for b in &order {
        log.push(trf("{b} を統合します", &[("b", b.clone())]));
        // rebase で基準を揃えてから fast-forward で載せる。**マージコミットを
        // 作らない**ので、統合先の履歴は 1 本のまま保たれる。
        let steps: [Vec<&str>; 4] = [
            vec!["checkout", "--quiet", b],
            vec!["rebase", &req.onto],
            vec!["checkout", "--quiet", &req.onto],
            vec!["merge", "--ff-only", b],
        ];
        let mut stopped = None;
        for argv in &steps {
            let Err(detail) = git_out(&top, argv) else {
                continue;
            };
            // **強行しない。** どこで衝突したかを取ってから畳む。
            let (files, lines) = conflict_details(&top);
            let _ = git_out(&top, &["rebase", "--abort"]);
            stopped = Some(TrainStop {
                branch: b.clone(),
                against: touchers(&touches, &files, &order, b),
                files,
                lines,
                detail,
                predicted: false,
            });
            break;
        }
        if let Some(stop) = stopped {
            log.push(trf(
                "⛔ {b} で止めました。強行はしません。",
                &[("b", b.clone())],
            ));
            report.restored = restore(&top, &saved, &head_ref, &head_oid, &mut log);
            report.merged.clear();
            report.stop = Some(stop);
            report.log = log;
            report.took_ms = t0.elapsed().as_millis();
            return Ok(report);
        }
        report.merged.push(b.clone());
    }

    // ⑦ 元いた場所へ戻す (統合先とブランチは進んだまま)。
    let back = match &head_ref {
        Some(r) => git_out(&top, &["checkout", "--quiet", "--force", r]),
        None => git_out(&top, &["checkout", "--quiet", "--force", "--detach", &head_oid]),
    };
    if back.is_err() {
        log.push(tr("⚠ 元のブランチへ戻れませんでした。git status を確認してください"));
    }
    log.push(trf(
        "✅ {n} 本すべてを {o} へ入れました",
        &[
            ("n", report.merged.len().to_string()),
            ("o", req.onto.clone()),
        ],
    ));
    report.log = log;
    report.took_ms = t0.elapsed().as_millis();
    Ok(report)
}

/// `files` を触っていて、`branch` より**先に入る**ブランチ (= 衝突の相手)。
fn touchers(
    touches: &[BranchTouch],
    files: &[String],
    order: &[String],
    branch: &str,
) -> Vec<String> {
    let before: Vec<&String> = order.iter().take_while(|b| b.as_str() != branch).collect();
    let want: BTreeSet<&str> = files.iter().map(String::as_str).collect();
    let mut out: Vec<String> = touches
        .iter()
        .filter(|t| before.iter().any(|b| *b == &t.branch))
        .filter(|t| t.files.iter().any(|f| want.contains(f.as_str())))
        .map(|t| t.branch.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

// ═══════════════════════════════════════════════════════════════════════
//  7. レイアウト — 純関数 (画面とテストが同じ関数を通る)
// ═══════════════════════════════════════════════════════════════════════

const EMPTY_CARD_MAX_W: f32 = 460.0;
const EMPTY_CARD_H: f32 = 132.0;

/// 空状態のカード。**利用可能領域の中央**に収める (下や上に取り残さない)。
pub fn empty_card(avail: egui::Rect) -> egui::Rect {
    let aw = avail.width().max(0.0);
    let ah = avail.height().max(0.0);
    let w = (aw - space::LG * 2.0).clamp(0.0, EMPTY_CARD_MAX_W).min(aw);
    let h = EMPTY_CARD_H.min(ah);
    let x = avail.left() + (aw - w) * 0.5;
    let y = avail.top() + (ah - h) * 0.5;
    egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h))
}

/// 1 行の列幅。**どの幅でも合計が可用幅を超えない。**
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowLayout {
    pub branch_w: f32,
    pub files_w: f32,
    pub overlap_w: f32,
    pub expect_w: f32,
    pub status_w: f32,
    pub gap: f32,
    /// 狭すぎてファイル数と重なりを畳んだか (ホバーで出す)。
    pub compact: bool,
}

const FILES_W: f32 = 52.0;
const STATUS_MIN: f32 = 76.0;
const STATUS_MAX: f32 = 120.0;
const BRANCH_MIN: f32 = 110.0;
const BRANCH_MAX: f32 = 240.0;
const OVERLAP_MIN: f32 = 90.0;
const EXPECT_MIN: f32 = 110.0;

/// 可用幅から列幅を決める (純関数)。
pub fn row_layout(avail_w: f32) -> RowLayout {
    let gap = space::SM;
    let w = avail_w.max(0.0);
    let fixed = FILES_W + gap * 4.0;
    if w < fixed + BRANCH_MIN + OVERLAP_MIN + EXPECT_MIN + STATUS_MIN {
        // 狭い: ブランチ / 予想 / 状態 の 3 列へ縮退する。
        let inner = (w - gap * 2.0).max(0.0);
        return RowLayout {
            branch_w: inner * 0.40,
            files_w: 0.0,
            overlap_w: 0.0,
            expect_w: inner * 0.34,
            status_w: inner * 0.26,
            gap,
            compact: true,
        };
    }
    let rest = w - fixed;
    let branch_w = (rest * 0.26).clamp(BRANCH_MIN, BRANCH_MAX);
    let status_w = (rest * 0.18).clamp(STATUS_MIN, STATUS_MAX);
    let free = (rest - branch_w - status_w).max(OVERLAP_MIN + EXPECT_MIN);
    let overlap_w = free * 0.45;
    let expect_w = free - overlap_w;
    RowLayout {
        branch_w,
        files_w: FILES_W,
        overlap_w,
        expect_w,
        status_w,
        gap,
        compact: false,
    }
}

/// 行の中の各セルの矩形 (純関数)。**必ず `row` に収まり、重ならない。**
pub fn row_rects(row: egui::Rect, lay: &RowLayout) -> Vec<egui::Rect> {
    let widths: Vec<f32> = if lay.compact {
        vec![lay.branch_w, lay.expect_w, lay.status_w]
    } else {
        vec![
            lay.branch_w,
            lay.files_w,
            lay.overlap_w,
            lay.expect_w,
            lay.status_w,
        ]
    };
    let mut out = Vec::with_capacity(widths.len());
    let mut x = row.left();
    for w in widths {
        let left = x.min(row.right());
        let right = (left + w.max(0.0)).min(row.right());
        out.push(egui::Rect::from_min_max(
            egui::pos2(left, row.top()),
            egui::pos2(right, row.bottom()),
        ));
        x = right + lay.gap;
    }
    out
}

/// 1 行に出す文言 (純関数)。**画面とテストが同じ関数を通る。**
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowCells {
    pub branch: String,
    pub files: String,
    pub overlap: String,
    pub expect: String,
    pub status: String,
    pub risk: Risk,
    /// ホバーに出す全文。
    pub hover: String,
}

/// 実行の進み具合。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunPhase {
    #[default]
    Idle,
    Running,
    Done,
}

/// 行の文言を組む (純関数)。
pub fn row_cells(
    step: &TrainStep,
    dry: &DryResult,
    report: Option<&TrainReport>,
    phase: RunPhase,
) -> RowCells {
    let mut files: Vec<String> = step
        .overlaps
        .iter()
        .flat_map(|o| o.files.iter().cloned())
        .collect();
    files.sort();
    files.dedup();
    let shown = files.len().min(MAX_SHOWN_FILES);
    let overlap = if step.overlaps.is_empty() {
        "—".to_string()
    } else {
        trf(
            "{n} 本 / {f} ファイル",
            &[
                ("n", step.overlaps.len().to_string()),
                ("f", files.len().to_string()),
            ],
        )
    };
    let dry_conflict = dry
        .steps
        .iter()
        .find(|d| d.branch == step.branch)
        .map(|d| !d.conflicts.is_empty())
        .unwrap_or(false);
    let expect = if dry_conflict {
        tr("⛔ 乾式検査で衝突")
    } else if !dry.available {
        format!("{} {}", step.expect.icon(), tr(step.expect.label()))
    } else if step.expect == Risk::Clear {
        format!("{} {}", Risk::Clear.icon(), tr("綺麗に入る"))
    } else {
        format!("{} {}", step.expect.icon(), tr(step.expect.label()))
    };
    let status = match (report, phase) {
        (_, RunPhase::Running) => tr("⏳ 実行中"),
        (Some(r), _) if r.merged.iter().any(|m| m == &step.branch) => tr("✅ 入った"),
        (Some(r), _) if r.stop.as_ref().is_some_and(|s| s.branch == step.branch) => {
            tr("⛔ 止まった")
        }
        (Some(r), _) if r.stop.is_some() => tr("— 未実行"),
        _ => tr("待機"),
    };
    let hover = if files.is_empty() {
        trf(
            "{b}: 他のブランチと 1 ファイルも被っていません",
            &[("b", step.branch.clone())],
        )
    } else {
        let mut s = files[..shown].join("\n");
        if files.len() > shown {
            s.push_str(&trf(
                "\nほか {n} ファイル",
                &[("n", (files.len() - shown).to_string())],
            ));
        }
        s
    };
    RowCells {
        branch: step.branch.clone(),
        files: step.files.to_string(),
        overlap,
        expect,
        status,
        risk: step.expect,
        hover,
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  8. パネル — `app.rs` を 1 バイトも触らずにウィンドウを出す
// ═══════════════════════════════════════════════════════════════════════

/// 走査 1 回ぶんの結果。**ウィンドウより長生きさせる** (設計原則 1)。
#[derive(Clone, Debug, Default)]
struct Snapshot {
    repo: PathBuf,
    onto: String,
    plan: TrainPlan,
    dry: DryResult,
    held: Vec<(String, PathBuf)>,
    note: Option<String>,
    cost: Duration,
}

#[derive(Default)]
struct PanelState {
    open: bool,
    root: PathBuf,
    snap: Snapshot,
    pending: Option<Receiver<Snapshot>>,
    last_scan: Option<Instant>,
    last_cost: Option<Duration>,
    run_rx: Option<Receiver<Result<TrainReport, String>>>,
    report: Option<TrainReport>,
    error: Option<String>,
}

fn state() -> &'static Mutex<PanelState> {
    static S: OnceLock<Mutex<PanelState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(PanelState::default()))
}

/// GUI が開いているワークスペース。取れなければカレントディレクトリ。
/// **パスは 1 つもハードコードしない。**
fn gui_workspace_root() -> PathBuf {
    let me = std::process::id();
    crate::instances::scan_and_prune(&crate::instances::instances_dir())
        .into_iter()
        .find(|e| e.pid == me)
        .and_then(|e| e.workspace_roots.first().map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// パレットからの入口。開閉を切り替える。
pub fn toggle_panel() {
    let opened = state().lock().map(|s| s.open).unwrap_or(false);
    let Ok(mut st) = state().lock() else { return };
    if opened {
        st.open = false;
        return;
    }
    st.open = true;
    st.root = gui_workspace_root();
    st.last_scan = None; // 開いた回は必ず取り直す
}

/// 1 回ぶんの走査 (**裏のスレッドで動く**)。
fn scan(root: PathBuf) -> Snapshot {
    let t0 = Instant::now();
    let Ok(top) = crate::worktree::repo_root(&root) else {
        return Snapshot {
            note: Some(tr("git リポジトリではありません")),
            cost: t0.elapsed(),
            ..Default::default()
        };
    };
    let onto = default_onto(&top);
    if onto.is_empty() {
        return Snapshot {
            repo: top,
            note: Some(tr("統合先のブランチが分かりません (HEAD が detached です)")),
            cost: t0.elapsed(),
            ..Default::default()
        };
    }
    let cand = candidates(&top, &onto);
    let touches = touches_from_repo(&top, &onto, &cand.free);
    let plan = plan_order(&touches);
    let dry = dry_run(&top, &onto, &plan.order());
    Snapshot {
        repo: top,
        onto,
        plan,
        dry,
        held: cand.held,
        note: None,
        cost: t0.elapsed(),
    }
}

fn spawn_scan(root: PathBuf) -> Option<Receiver<Snapshot>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("zv-train-scan".into())
        .spawn(move || {
            let _ = tx.send(scan(root));
        })
        .ok()
        .map(|_| rx)
}

/// 統合を裏で始める。**UI スレッドは 1 ミリ秒も待たない。**
fn start_run(st: &mut PanelState) {
    if st.run_rx.is_some() {
        return;
    }
    let req = TrainRequest {
        repo: st.snap.repo.clone(),
        onto: st.snap.onto.clone(),
        branches: st.snap.plan.order(),
        dry_run: false,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("zv-train-run".into())
        .spawn(move || {
            let _ = tx.send(run_train(&req));
        });
    if spawned.is_ok() {
        st.run_rx = Some(rx);
        st.error = None;
    }
}

/// 非同期の結果を拾い、必要なら次の走査を出す。**待たない。**
fn poll(st: &mut PanelState, ctx: &egui::Context) {
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
    if let Some(rx) = &st.run_rx {
        match rx.try_recv() {
            Ok(Ok(r)) => {
                st.report = Some(r);
                st.run_rx = None;
                st.last_scan = None; // 参照が動いたので取り直す
            }
            Ok(Err(e)) => {
                st.error = Some(e);
                st.run_rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => st.run_rx = None,
        }
    }
    if st.pending.is_none() && st.run_rx.is_none() {
        let due = st
            .last_scan
            .is_none_or(|t| t.elapsed() >= crate::git::scan_interval(SCAN_BASE, st.last_cost));
        if due {
            st.pending = spawn_scan(st.root.clone());
            if st.pending.is_none() {
                st.last_scan = Some(Instant::now());
            }
        }
    }
    // 開いている間だけ、結果を拾うために軽く回す。
    ctx.request_repaint_after(Duration::from_millis(400));
}

/// パネルから返る操作。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Act {
    #[default]
    None,
    /// 乾式検査 (= 走査をやり直す)。
    Dry,
    /// 統合を開始する。
    Run,
}

/// 毎フレーム呼ばれる描画。**閉じているフレームは 1 ピクセルも触らない**
/// (設計原則 3: アイドル時のコストはゼロ)。
///
/// **ここから git を撃たない。** 表示するのは常に「いま手元にある値」で、
/// 1 テンポ古くてよい。番人テスト `描画から同期gitを撃たない` がある。
pub fn draw(app: &mut crate::app::ZaivernApp, ctx: &egui::Context) {
    let _ = app; // 状態はモジュール側に持つので app の中身へは触らない
    let Ok(mut st) = state().lock() else { return };
    if !st.open {
        return;
    }
    poll(&mut st, ctx);
    let mut open = true;
    let mut act = Act::None;
    egui::Window::new(tr("🚃 マージトレイン — 並列の成果を順に統合する"))
        .collapsible(false)
        .resizable(true)
        .default_width(720.0)
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
        Act::Dry => st.last_scan = None,
        Act::Run => start_run(&mut st),
        Act::None => {}
    }
}

/// 本体。押された操作を返す。
fn body(ui: &mut egui::Ui, st: &PanelState) -> Act {
    let mut act = Act::None;
    let vis = ui.visuals().clone();
    let dim = vis.weak_text_color();
    let phase = if st.run_rx.is_some() {
        RunPhase::Running
    } else if st.report.is_some() {
        RunPhase::Done
    } else {
        RunPhase::Idle
    };

    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new(trf(
                "{n} 本を {o} へ",
                &[
                    ("n", st.snap.plan.steps.len().to_string()),
                    (
                        "o",
                        if st.snap.onto.is_empty() {
                            "?".to_string()
                        } else {
                            st.snap.onto.clone()
                        },
                    ),
                ],
            ))
            .strong(),
        );
        if st.snap.plan.line_pairs > 0 {
            ui.label(
                egui::RichText::new(trf(
                    "行が重なる組 {n}",
                    &[("n", st.snap.plan.line_pairs.to_string())],
                ))
                .color(vis.warn_fg_color)
                .small(),
            )
            .on_hover_text(tr(
                "順序をどう変えても消えない重なりです。人手が要る回数の下限になります。",
            ));
        }
        if let Some(c) = st.last_cost {
            ui.label(
                egui::RichText::new(format!("{} ms", c.as_millis()))
                    .color(dim)
                    .small(),
            )
            .on_hover_text(tr("走査は裏のスレッドで行うので、UI は止まりません"));
        }
        if st.pending.is_some() || st.run_rx.is_some() {
            ui.spinner();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let busy = st.run_rx.is_some();
            let ready = !st.snap.plan.steps.is_empty() && !st.snap.onto.is_empty();
            if ui
                .add_enabled(!busy && ready, egui::Button::new(tr("統合を開始")))
                .on_hover_text(tr(
                    "順に rebase して fast-forward します。失敗したら即止めて、開始時の状態へ全部戻します。",
                ))
                .clicked()
            {
                act = Act::Run;
            }
            if ui
                .add_enabled(!busy, egui::Button::new(tr("乾式検査")))
                .on_hover_text(tr("参照を 1 つも動かさずに、この順序で衝突するかを見ます"))
                .clicked()
            {
                act = Act::Dry;
            }
        });
    });

    for note in [st.snap.note.as_ref(), st.snap.dry.note.as_ref()]
        .into_iter()
        .flatten()
    {
        ui.label(egui::RichText::new(format!("ℹ {note}")).small().color(dim));
    }
    if let Some(e) = &st.error {
        ui.colored_label(vis.error_fg_color, format!("⛔ {e}"));
    }
    if !st.snap.held.is_empty() {
        let list = st
            .snap
            .held
            .iter()
            .map(|(b, _)| b.clone())
            .collect::<Vec<_>>()
            .join(", ");
        ui.label(
            egui::RichText::new(trf(
                "別のワークツリーが使用中のため外したブランチ: {list}",
                &[("list", list.clone())],
            ))
            .small()
            .color(dim),
        )
        .on_hover_text(tr(
            "作業中のエージェントの足元で履歴を書き換えないため、対象から外しています。",
        ));
    }
    if st.snap.plan.dropped > 0 {
        ui.colored_label(
            vis.warn_fg_color,
            trf(
                "⚠ 上限 {n} 本で打ち切りました (残り {m} 本)",
                &[
                    ("n", MAX_BRANCHES.to_string()),
                    ("m", st.snap.plan.dropped.to_string()),
                ],
            ),
        );
    }

    if st.snap.plan.steps.is_empty() {
        empty_state(ui, st);
        return act;
    }

    ui.separator();
    egui::ScrollArea::vertical()
        .id_salt("zv-train-rows")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let lay = row_layout(ui.available_width());
            for step in &st.snap.plan.steps {
                let cells = row_cells(step, &st.snap.dry, st.report.as_ref(), phase);
                row(ui, &cells, &lay, &vis);
            }
            if let Some(stop) = st.report.as_ref().and_then(|r| r.stop.as_ref()) {
                stop_detail(ui, stop, st.report.as_ref().map(|r| r.restored));
            }
        });
    act
}

/// 1 行。**どの幅でも見切れない** — セルの矩形は [`row_rects`] が決めるので、
/// 画面に出る位置と「収まり・重ならない」を検査するテストが同じ関数を通る。
fn row(ui: &mut egui::Ui, c: &RowCells, lay: &RowLayout, vis: &egui::Visuals) {
    let h = ui.text_style_height(&egui::TextStyle::Body);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
    let cells = row_rects(rect, lay);
    let color = match c.risk {
        Risk::Clear => vis.text_color(),
        Risk::File => vis.warn_fg_color,
        Risk::Line => vis.error_fg_color,
    };
    let texts: Vec<egui::RichText> = if lay.compact {
        vec![
            egui::RichText::new(&c.branch).strong(),
            egui::RichText::new(&c.expect).color(color),
            egui::RichText::new(&c.status),
        ]
    } else {
        vec![
            egui::RichText::new(&c.branch).strong(),
            egui::RichText::new(&c.files).color(vis.weak_text_color()),
            egui::RichText::new(&c.overlap).color(vis.weak_text_color()),
            egui::RichText::new(&c.expect).color(color),
            egui::RichText::new(&c.status),
        ]
    };
    for (i, t) in texts.into_iter().enumerate() {
        let Some(cell) = cells.get(i) else { continue };
        // 幅ゼロのセルには何も描かない (空白を作らない)。
        if cell.width() <= 1.0 {
            continue;
        }
        ui.put(*cell, egui::Label::new(t).truncate())
            .on_hover_text(&c.hover);
    }
}

/// 止まったときの詳細。**どのファイルの・どの行で・誰と**を出す。
fn stop_detail(ui: &mut egui::Ui, stop: &TrainStop, restored: Option<bool>) {
    let vis = ui.visuals().clone();
    ui.separator();
    ui.colored_label(
        vis.error_fg_color,
        trf("⛔ {b} で止めました", &[("b", stop.branch.clone())]),
    );
    if !stop.against.is_empty() {
        ui.label(
            egui::RichText::new(trf(
                "相手: {list}",
                &[("list", stop.against.join(", "))],
            ))
            .small()
            .color(vis.weak_text_color()),
        );
    }
    let mut where_: Vec<String> = stop.lines.clone();
    if where_.is_empty() {
        where_ = stop.files.clone();
    }
    for w in where_.iter().take(MAX_SHOWN_FILES) {
        ui.label(
            egui::RichText::new(w)
                .small()
                .monospace()
                .color(vis.weak_text_color()),
        );
    }
    if where_.len() > MAX_SHOWN_FILES {
        ui.label(
            egui::RichText::new(trf(
                "ほか {n} 件",
                &[("n", (where_.len() - MAX_SHOWN_FILES).to_string())],
            ))
            .small()
            .color(vis.weak_text_color()),
        );
    }
    if let Some(ok) = restored {
        let (txt, col) = if ok {
            (
                tr("開始時の状態へ戻しました (何も動いていません)"),
                vis.weak_text_color(),
            )
        } else {
            (
                tr("⚠ 戻し切れませんでした。git status を確認してください"),
                vis.error_fg_color,
            )
        };
        ui.label(egui::RichText::new(txt).small().color(col));
    }
}

/// 空状態。**利用可能領域の中央に 1 枚のカード**で出す。
fn empty_state(ui: &mut egui::Ui, st: &PanelState) {
    let vis = ui.visuals().clone();
    let avail = ui.available_rect_before_wrap().intersect(ui.clip_rect());
    let card = empty_card(avail);
    let held = st.snap.held.len();
    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(card), |ui| {
        egui::Frame::none()
            .fill(vis.faint_bg_color)
            .stroke(egui::Stroke::new(
                1.0_f32,
                vis.widgets.noninteractive.bg_stroke.color,
            ))
            .rounding(egui::Rounding::same(10.0))
            .inner_margin(egui::Margin::same(space::MD))
            .show(ui, |ui| {
                ui.set_width((card.width() - space::MD * 2.0).max(0.0));
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new(tr("統合を待っているブランチはありません")).size(14.0));
                    ui.add_space(space::XS);
                    ui.label(
                        egui::RichText::new(if held > 0 {
                            trf(
                                "{n} 本は別のワークツリーが使用中なので外しています",
                                &[("n", held.to_string())],
                            )
                        } else {
                            tr("統合先より先に進んでいるローカルブランチだけを載せます")
                        })
                        .small()
                        .color(vis.weak_text_color()),
                    );
                });
            });
    });
}

// ═══════════════════════════════════════════════════════════════════════
//  9. CLI — `zai train <sub>`
// ═══════════════════════════════════════════════════════════════════════

fn usage() -> String {
    [
        tr("使い方: zai train <サブコマンド>"),
        String::new(),
        tr("  plan [--repo <path>] [--json]"),
        tr("      統合順と、その順序で予想される衝突を出す (参照は動かさない)"),
        tr("  run  [--repo <path>] [--onto <branch>] [--dry-run]"),
        tr("      順に rebase して fast-forward する。失敗したら全部戻す"),
        String::new(),
        tr("終了コード: 0=成功 / 1=衝突で停止 / 2=使い方の誤り"),
        String::new(),
    ]
    .join("\n")
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Flags {
    repo: PathBuf,
    onto: Option<String>,
    json: bool,
    dry: bool,
}

/// 引数を読む。**知らないフラグは黙って無視しない** (使い方の誤りは 2)。
fn parse_flags(args: &[String], allow_dry: bool) -> Result<Flags, String> {
    let mut f = Flags {
        repo: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        ..Default::default()
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--repo" => {
                let Some(v) = it.next() else {
                    return Err("--repo にパスがありません".into());
                };
                f.repo = PathBuf::from(v);
            }
            "--onto" => {
                let Some(v) = it.next() else {
                    return Err("--onto にブランチ名がありません".into());
                };
                f.onto = Some(v.clone());
            }
            "--json" => f.json = true,
            "--dry-run" if allow_dry => f.dry = true,
            other => return Err(format!("知らない引数: {other}")),
        }
    }
    Ok(f)
}

/// `zai train <sub>` の実体。argv は `"train"` の**次**から渡される。
/// 戻り値は終了コード (0=成功 / 1=衝突で停止 / 2=使い方の誤り)。
///
/// **`src/cli.rs` は共有ファイルなので触っていない。** 配線 (`"train" =>
/// train::cli_main(&args[1..])` の 1 行) は統合担当が入れる。それまでこの
/// `zai train <sub>` の実体。`src/cli.rs` の dispatch から呼ばれる。
pub fn cli_main(argv: &[String]) -> i32 {
    let Some(sub) = argv.first().map(String::as_str) else {
        print!("{}", usage());
        return 2;
    };
    match sub {
        "help" | "--help" | "-h" => {
            print!("{}", usage());
            0
        }
        "plan" => cli_plan(&argv[1..]),
        "run" => cli_run(&argv[1..]),
        other => {
            eprintln!(
                "{}",
                trf(
                    "zai train: 知らないサブコマンド {s}",
                    &[("s", other.to_string())]
                )
            );
            print!("{}", usage());
            2
        }
    }
}

/// `plan --json` の出力形。
#[derive(Serialize)]
struct PlanOut {
    onto: String,
    plan: TrainPlan,
    dry: DryResult,
    held: Vec<String>,
}

fn cli_plan(args: &[String]) -> i32 {
    let f = match parse_flags(args, false) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            print!("{}", usage());
            return 2;
        }
    };
    let top = match crate::worktree::repo_root(&f.repo) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let onto = f.onto.unwrap_or_else(|| default_onto(&top));
    if onto.is_empty() {
        eprintln!("{}", tr("統合先のブランチが分かりません (--onto で指定してください)"));
        return 2;
    }
    let cand = candidates(&top, &onto);
    let touches = touches_from_repo(&top, &onto, &cand.free);
    let plan = plan_order(&touches);
    let dry = dry_run(&top, &onto, &plan.order());
    let out = PlanOut {
        onto,
        plan,
        dry,
        held: cand.held.into_iter().map(|(b, _)| b).collect(),
    };
    if f.json {
        match serde_json::to_string_pretty(&out) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        }
        return 0;
    }
    println!("{}", trf("統合先: {o}", &[("o", out.onto.clone())]));
    if out.plan.steps.is_empty() {
        println!("{}", tr("統合を待っているブランチはありません"));
    }
    for s in &out.plan.steps {
        let c = row_cells(s, &out.dry, None, RunPhase::Idle);
        println!("{:>2}. {:<28} {:>4}  {}", s.position + 1, c.branch, c.files, c.expect);
    }
    if !out.held.is_empty() {
        println!(
            "{}",
            trf(
                "別のワークツリーが使用中: {list}",
                &[("list", out.held.join(", "))]
            )
        );
    }
    if let Some(n) = &out.dry.note {
        println!("{n}");
    }
    0
}

fn cli_run(args: &[String]) -> i32 {
    let f = match parse_flags(args, true) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            print!("{}", usage());
            return 2;
        }
    };
    let top = match crate::worktree::repo_root(&f.repo) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let onto = f.onto.unwrap_or_else(|| default_onto(&top));
    if onto.is_empty() {
        eprintln!("{}", tr("統合先のブランチが分かりません (--onto で指定してください)"));
        return 2;
    }
    let branches = candidates(&top, &onto).free;
    let req = TrainRequest {
        repo: top,
        onto,
        branches,
        dry_run: f.dry,
    };
    let report = match run_train(&req) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if f.json {
        match serde_json::to_string_pretty(&report) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        }
    } else {
        for l in &report.log {
            println!("{l}");
        }
        if let Some(stop) = &report.stop {
            for w in stop.lines.iter().chain(stop.files.iter()).take(MAX_SHOWN_FILES) {
                println!("  {w}");
            }
            if !stop.against.is_empty() {
                println!("  {}", trf("相手: {l}", &[("l", stop.against.join(", "))]));
            }
        }
    }
    if report.ok() {
        0
    } else {
        1
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  10. 登録 — 共有ファイルを 1 バイトも触らずに機能が繋がる入口
// ═══════════════════════════════════════════════════════════════════════

/// パレットへの登録。
///
/// 打鍵は割り当てていない — `keybinds::BindAction` は固定長配列 + 件数検査を
/// 持つ最も硬い共有面で、機能ブランチ側から増やすと直列マージが必ず衝突する。
/// **欲しい打鍵があれば統合担当へ報告して直列に入れてもらう。**
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "train",
    entries: &[crate::feature::Entry {
        icon: "🚃",
        label: "マージトレイン (順次統合)",
        id: "train.open",
    }],
    dispatch: |_app, _ctx, id| match id {
        "train.open" => {
            toggle_panel();
            true
        }
        _ => false,
    },
    // 窓は中央ビューに属さないオーバーレイなので、毎フレームここから描く。
    // **閉じているときは 1 命令も走らない** (`draw` の先頭で即 return する)
    // ので、アイドル時のコストはゼロのまま。
    draw: Some(draw),
    settings: &[],
    binds: &[],
};

// ═══════════════════════════════════════════════════════════════════════
//  11. テスト
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── 純関数: 順序決定 ────────────────────────────────────────────

    fn touch(branch: &str, files: &[&str]) -> BranchTouch {
        BranchTouch {
            branch: branch.into(),
            files: files.iter().map(|f| (*f).to_string()).collect(),
            spans: BTreeMap::new(),
        }
    }

    fn touch_lines(branch: &str, file: &str, from: usize, to: usize) -> BranchTouch {
        let mut t = touch(branch, &[file]);
        t.spans.insert(file.into(), vec![Span::edit(from, to)]);
        t
    }

    #[test]
    fn 順序決定のテーブル() {
        // (名前, 入力, 期待する順序)
        let cases: Vec<(&str, Vec<BranchTouch>, Vec<&str>)> = vec![
            ("空", vec![], vec![]),
            ("1 本", vec![touch("solo", &["a.rs"])], vec!["solo"]),
            (
                "全部独立 — 同点なので辞書順",
                vec![
                    touch("c", &["c.rs"]),
                    touch("a", &["a.rs"]),
                    touch("b", &["b.rs"]),
                ],
                vec!["a", "b", "c"],
            ),
            (
                "全部重なる — 同点なので辞書順",
                vec![
                    touch("z", &["same.rs"]),
                    touch("y", &["same.rs"]),
                    touch("x", &["same.rs"]),
                ],
                vec!["x", "y", "z"],
            ),
            (
                // hub は 2 本と被る / lone は誰とも被らない。
                // ta を入れたあと hub と tb は「残り 1 本と重なる」で並ぶので、
                // **足跡の広い hub が後ろ**へ回る (名前だけで決めると hub が先に出る)。
                "重なりが少ないものが先",
                vec![
                    touch("hub", &["a.rs", "b.rs"]),
                    touch("ta", &["a.rs"]),
                    touch("tb", &["b.rs"]),
                    touch("lone", &["z.rs"]),
                ],
                vec!["lone", "ta", "tb", "hub"],
            ),
            (
                "同点はブランチ名の辞書順で割る",
                vec![touch("bbb", &["x.rs"]), touch("aaa", &["y.rs"])],
                vec!["aaa", "bbb"],
            ),
        ];
        for (name, input, want) in cases {
            let got = plan_order(&input).order();
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn 順序は入力の並びに依存しない() {
        let mut a = vec![
            touch("hub", &["a.rs", "b.rs"]),
            touch("ta", &["a.rs"]),
            touch("tb", &["b.rs"]),
            touch("lone", &["z.rs"]),
        ];
        let want = plan_order(&a).order();
        // 入れ替えても同じ計画が出る (HashMap の反復順を漏らしていない証拠)。
        for _ in 0..4 {
            a.rotate_left(1);
            assert_eq!(plan_order(&a).order(), want);
            let mut rev = a.clone();
            rev.reverse();
            assert_eq!(plan_order(&rev).order(), want);
        }
    }

    #[test]
    fn 行が重なるかどうかで危険度が変わる() {
        // 同じファイルの別の行 → File
        let a = touch_lines("a", "f.rs", 1, 3);
        let b = touch_lines("b", "f.rs", 100, 120);
        assert_eq!(risk_between(&a, &b).risk, Risk::File);
        // 同じ行域 → Line
        let c = touch_lines("c", "f.rs", 2, 10);
        assert_eq!(risk_between(&a, &c).risk, Risk::Line);
        // ファイルが違う → Clear
        let d = touch_lines("d", "g.rs", 1, 3);
        assert_eq!(risk_between(&a, &d).risk, Risk::Clear);
        // 行が分からない側があれば File 止まり (言い切れないことは言わない)
        let e = touch("e", &["f.rs"]);
        assert_eq!(risk_between(&a, &e).risk, Risk::File);
    }

    #[test]
    fn 行の重なりは順序と予想へ反映される() {
        let input = vec![
            touch_lines("a", "f.rs", 1, 10),
            touch_lines("b", "f.rs", 5, 20),
            touch("c", &["other.rs"]),
        ];
        let plan = plan_order(&input);
        assert_eq!(plan.line_pairs, 1);
        assert_eq!(plan.order()[0], "c", "誰とも被らないものが先");
        let last = plan.steps.last().expect("3 段ある");
        assert_eq!(last.expect, Risk::Line);
        assert_eq!(last.overlaps.len(), 1);
    }

    #[test]
    fn 上限を超えたぶんは黙って捨てずに数える() {
        let input: Vec<BranchTouch> = (0..MAX_BRANCHES + 3)
            .map(|i| touch(&format!("b{i:03}"), &[&format!("f{i}.rs")]))
            .collect();
        let plan = plan_order(&input);
        assert_eq!(plan.steps.len(), MAX_BRANCHES);
        assert_eq!(plan.dropped, 3);
    }

    // ── 純関数: レイアウト ──────────────────────────────────────────

    #[test]
    fn 空状態カードは可用領域の中央に必ず収まる() {
        for (w, h) in [
            (900.0_f32, 700.0_f32),
            (1200.0, 300.0),
            (320.0, 200.0),
            (100.0, 60.0),
            (0.0, 0.0),
        ] {
            let avail = egui::Rect::from_min_size(egui::pos2(9.0, 21.0), egui::vec2(w, h));
            let card = empty_card(avail);
            assert!(
                avail.contains_rect(card),
                "{w}x{h} でカードがはみ出した: {card:?}"
            );
            if w > 0.0 && h > 0.0 {
                assert!((card.center().x - avail.center().x).abs() < 0.01);
                assert!((card.center().y - avail.center().y).abs() < 0.01);
            }
        }
    }

    #[test]
    fn 行のセルはどの幅でも領域に収まり重ならない() {
        for w in [900.0_f32, 1200.0, 640.0, 420.0, 300.0, 160.0, 40.0, 0.0] {
            let lay = row_layout(w);
            let row = egui::Rect::from_min_size(egui::pos2(5.0, 5.0), egui::vec2(w, 20.0));
            let cells = row_rects(row, &lay);
            assert!(!cells.is_empty(), "幅 {w} でセルが 0 個");
            for c in &cells {
                assert!(row.contains_rect(*c), "幅 {w} ではみ出した: {c:?}");
                assert!(c.width() >= 0.0);
            }
            for pair in cells.windows(2) {
                assert!(
                    pair[0].right() <= pair[1].left() + 0.01,
                    "幅 {w} で列が重なった: {:?} / {:?}",
                    pair[0],
                    pair[1]
                );
            }
        }
    }

    #[test]
    fn 狭い幅では列を畳む() {
        assert!(!row_layout(900.0).compact);
        assert!(row_layout(240.0).compact);
        assert_eq!(row_layout(240.0).overlap_w, 0.0);
        assert_eq!(row_rects(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(240.0, 18.0)), &row_layout(240.0)).len(), 3);
    }

    #[test]
    fn 行の文言は状態を取り違えない() {
        let plan = plan_order(&[
            touch("a", &["f.rs"]),
            touch("b", &["f.rs"]),
            touch("c", &["z.rs"]),
        ]);
        let dry = DryResult {
            available: true,
            steps: plan
                .order()
                .iter()
                .map(|b| DryStep {
                    branch: b.clone(),
                    conflicts: Vec::new(),
                })
                .collect(),
            note: None,
        };
        let mut report = TrainReport {
            merged: vec![plan.order()[0].clone()],
            ..Default::default()
        };
        report.stop = Some(TrainStop {
            branch: plan.order()[1].clone(),
            ..Default::default()
        });
        let c0 = row_cells(&plan.steps[0], &dry, Some(&report), RunPhase::Done);
        let c1 = row_cells(&plan.steps[1], &dry, Some(&report), RunPhase::Done);
        let c2 = row_cells(&plan.steps[2], &dry, Some(&report), RunPhase::Done);
        assert!(c0.status.contains("入った"));
        assert!(c1.status.contains("止まった"));
        assert!(c2.status.contains("未実行"));
        // 実行中は全行が「実行中」になる (取り違えない)
        let r = row_cells(&plan.steps[0], &dry, Some(&report), RunPhase::Running);
        assert!(r.status.contains("実行中"));
    }

    // ── 番人 ────────────────────────────────────────────────────────

    /// **UI スレッドで git を待たない**という約束の番人。
    #[test]
    fn 描画から同期gitを撃たない() {
        let src = include_str!("train.rs").replace("\r\n", "\n");
        // 探す文字列をそのまま書くと**このテスト自身に当たる**ので分割する。
        let draw_sig = concat!(
            "pub fn draw(app: &mut crate::app::",
            "ZaivernApp, ctx: &egui::Context) {"
        );
        let body_sig = concat!("fn body(ui: &mut egui::Ui, st: &", "PanelState) -> Act {");
        for sig in [draw_sig, body_sig] {
            let body = src
                .split(sig)
                .nth(1)
                .unwrap_or_else(|| panic!("{sig} が見つからない"));
            let body = body.split("\n}\n").next().expect("本体の終端");
            for bad in [
                "git_out(",
                "run_train(",
                "dry_run(",
                "touches_from_repo(",
                "candidates(",
                ".output()",
            ] {
                assert!(!body.contains(bad), "{sig} が同期 git を撃っている: {bad}");
            }
        }
        assert!(
            src.contains("std::thread::Builder::new()"),
            "git を裏のスレッドへ逃がしていない"
        );
        assert!(
            src.contains("crate::git::scan_interval"),
            "スキャン間隔を適応させていない"
        );
    }

    #[test]
    fn 登録の形が崩れていない() {
        assert_eq!(FEATURE.module, "train");
        assert!(!FEATURE.entries.is_empty());
        for e in FEATURE.entries {
            assert!(e.id.starts_with("train."), "ID がずれている: {}", e.id);
            assert!(e.id.len() > "train.".len(), "動作名が空: {}", e.id);
        }
        assert!(FEATURE.draw.is_some(), "描画が繋がっていない");
    }

    // ── 実リポジトリを使った統合テスト ──────────────────────────────

    struct Repo(PathBuf);

    impl Drop for Repo {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    impl Repo {
        fn git(&self, args: &[&str]) -> String {
            git_out(&self.0, args)
                .unwrap_or_else(|e| panic!("git {args:?} が失敗した: {e}"))
        }

        fn write(&self, rel: &str, text: &str) {
            let p = self.0.join(rel);
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).expect("親ディレクトリ");
            }
            std::fs::write(p, text).expect("書き込み");
        }

        fn commit(&self, rel: &str, text: &str, msg: &str) {
            self.write(rel, text);
            self.git(&["add", "--all"]);
            self.git(&["commit", "--quiet", "-m", msg]);
        }

        fn oid(&self, r: &str) -> String {
            self.git(&["rev-parse", r])
        }
    }

    /// 実 git リポジトリを作る。**実 `~/.zaivern` には触れない。**
    fn make_repo(tag: &str) -> Option<Repo> {
        if git_out(Path::new("."), &["--version"]).is_err() {
            return None; // git が無い環境ではこのテストを飛ばす
        }
        let dir = crate::test_util::unique_temp_dir("zv-train", tag);
        // 既定ブランチ名は git の版で変わるので**必ず自分で決める**
        // (ハードコードした "main" / "master" のどちらにも依存しない)。
        if git_out(&dir, &["init", "--quiet", "--initial-branch=base"]).is_err() {
            git_out(&dir, &["init", "--quiet"]).ok()?;
            git_out(&dir, &["checkout", "--quiet", "-b", "base"]).ok()?;
        }
        let r = Repo(dir);
        r.git(&["config", "user.name", "zaivern test"]);
        r.git(&["config", "user.email", "test@zaivern.invalid"]);
        r.git(&["config", "commit.gpgsign", "false"]);
        r.commit("shared.txt", &lines(1, 60), "base");
        r.commit("solo.txt", "solo\n", "solo");
        Some(r)
    }

    fn lines(from: usize, to: usize) -> String {
        (from..=to)
            .map(|i| format!("line {i}\n"))
            .collect::<String>()
    }

    /// `shared.txt` の `at` 行目だけを差し替えた本文。
    fn edited(at: usize, text: &str) -> String {
        (1..=60)
            .map(|i| {
                if i == at {
                    format!("{text}\n")
                } else {
                    format!("line {i}\n")
                }
            })
            .collect()
    }

    #[test]
    fn 三本のトレインが順序どおり全部入る() {
        let Some(r) = make_repo("all-in") else { return };
        // b1 / b2 は同じファイルの遠い行、b3 は別ファイル。
        r.git(&["checkout", "--quiet", "-b", "b1"]);
        r.commit("shared.txt", &edited(3, "b1 here"), "b1");
        r.git(&["checkout", "--quiet", "base"]);
        r.git(&["checkout", "--quiet", "-b", "b2"]);
        r.commit("shared.txt", &edited(55, "b2 here"), "b2");
        r.git(&["checkout", "--quiet", "base"]);
        r.git(&["checkout", "--quiet", "-b", "b3"]);
        r.commit("solo.txt", "b3 only\n", "b3");
        r.git(&["checkout", "--quiet", "base"]);

        let cand = candidates(&r.0, "base");
        assert_eq!(cand.free, vec!["b1", "b2", "b3"], "候補が揃っている");
        assert!(cand.held.is_empty());

        let req = TrainRequest {
            repo: r.0.clone(),
            onto: "base".into(),
            branches: cand.free.clone(),
            dry_run: false,
        };
        let rep = run_train(&req).expect("実行できる");
        assert!(rep.ok(), "止まった: {:?}", rep.stop);
        // b3 は誰とも被らないので先頭。
        assert_eq!(rep.plan.order()[0], "b3");
        assert_eq!(rep.merged, rep.plan.order(), "順序どおりに全部入った");
        // 3 本ぶんの変更が統合先に載っている。
        let head = r.git(&["show", "base:shared.txt"]);
        assert!(head.contains("b1 here") && head.contains("b2 here"));
        assert_eq!(r.git(&["show", "base:solo.txt"]).trim(), "b3 only");
        // 元いたブランチへ戻っている。
        assert_eq!(r.git(&["symbolic-ref", "--short", "HEAD"]).trim(), "base");
        assert!(r.git(&["status", "--porcelain"]).trim().is_empty());
    }

    #[test]
    fn 衝突は乾式検査が参照を動かす前に止める() {
        let Some(r) = make_repo("dry-stop") else { return };
        r.git(&["checkout", "--quiet", "-b", "c1"]);
        r.commit("shared.txt", &edited(10, "c1 wins"), "c1");
        r.git(&["checkout", "--quiet", "base"]);
        r.git(&["checkout", "--quiet", "-b", "c2"]);
        r.commit("shared.txt", &edited(10, "c2 wins"), "c2");
        r.git(&["checkout", "--quiet", "base"]);

        let before = (r.oid("base"), r.oid("c1"), r.oid("c2"));
        let req = TrainRequest {
            repo: r.0.clone(),
            onto: "base".into(),
            branches: vec!["c1".into(), "c2".into()],
            dry_run: false,
        };
        let rep = run_train(&req).expect("実行できる");
        let stop = rep.stop.as_ref().expect("止まる");
        assert!(stop.predicted, "参照を動かす前に予告して止めた");
        assert_eq!(stop.branch, "c2", "2 番目で衝突する");
        assert_eq!(stop.files, vec!["shared.txt"]);
        assert_eq!(stop.against, vec!["c1"], "相手が分かる");
        assert!(rep.merged.is_empty());
        assert!(rep.restored);
        // **参照は 1 つも動いていない。**
        assert_eq!((r.oid("base"), r.oid("c1"), r.oid("c2")), before);
        assert!(r.git(&["status", "--porcelain"]).trim().is_empty());
    }

    /// 乾式検査 (最終形の三方向マージ) では綺麗なのに、rebase は途中の
    /// コミットで衝突する — **本番の fail-closed と全戻しの経路**。
    #[test]
    fn 乾式をすり抜けた衝突は実行時に止まり元へ戻る() {
        let Some(r) = make_repo("run-stop") else { return };
        // 統合先が 10 行目を書き換える。
        r.commit("shared.txt", &edited(10, "base moved"), "base moves");
        // ブランチは 10 行目を一度触ってから元へ戻す。
        // → 最終形の差分はゼロなので merge-tree は「綺麗」と言うが、
        //   rebase は 1 つ目のコミットを当て直すので衝突する。
        r.git(&["checkout", "--quiet", "-b", "flip", "HEAD~1"]);
        r.commit("shared.txt", &edited(10, "flip touched"), "flip 1");
        r.commit("shared.txt", &edited(10, "line 10"), "flip 2");
        r.git(&["checkout", "--quiet", "base"]);

        let before = (r.oid("base"), r.oid("flip"));
        let dry = dry_run(&r.0, "base", &["flip".to_string()]);
        if !dry.available {
            return; // git 2.38 未満では乾式が無いので、この筋書きは作れない
        }
        assert!(
            dry.first_conflict().is_none(),
            "この筋書きは乾式をすり抜ける前提"
        );

        let req = TrainRequest {
            repo: r.0.clone(),
            onto: "base".into(),
            branches: vec!["flip".into()],
            dry_run: false,
        };
        let rep = run_train(&req).expect("実行できる");
        let stop = rep.stop.as_ref().expect("実行時に止まる");
        assert!(!stop.predicted, "乾式では分からなかった");
        assert_eq!(stop.branch, "flip");
        assert_eq!(stop.files, vec!["shared.txt"]);
        assert!(
            stop.lines.iter().any(|l| l.starts_with("shared.txt:")),
            "どの行で衝突したかを持つ: {:?}",
            stop.lines
        );
        assert!(rep.merged.is_empty(), "強行していない");
        assert!(rep.restored, "戻せた");
        // **開始時の状態へ全部戻っている。**
        assert_eq!((r.oid("base"), r.oid("flip")), before);
        assert_eq!(r.git(&["symbolic-ref", "--short", "HEAD"]).trim(), "base");
        assert!(r.git(&["status", "--porcelain"]).trim().is_empty());
        // rebase の途中で止まっていない (`.git/rebase-merge` が残っていない)。
        assert!(!r.0.join(".git/rebase-merge").exists());
        assert!(!r.0.join(".git/rebase-apply").exists());
    }

    #[test]
    fn 作業ツリーが汚れていたら始めない() {
        let Some(r) = make_repo("dirty") else { return };
        r.git(&["checkout", "--quiet", "-b", "d1"]);
        r.commit("solo.txt", "d1\n", "d1");
        r.git(&["checkout", "--quiet", "base"]);
        r.write("shared.txt", "dirty\n");
        let before = r.oid("base");
        let err = run_train(&TrainRequest {
            repo: r.0.clone(),
            onto: "base".into(),
            branches: vec!["d1".into()],
            dry_run: false,
        })
        .expect_err("汚れていたら拒否する");
        assert!(err.contains("未コミット"), "{err}");
        assert_eq!(r.oid("base"), before);
    }

    #[test]
    fn 乾式指定なら参照を動かさない() {
        let Some(r) = make_repo("dry-only") else { return };
        r.git(&["checkout", "--quiet", "-b", "e1"]);
        r.commit("solo.txt", "e1\n", "e1");
        r.git(&["checkout", "--quiet", "base"]);
        let before = (r.oid("base"), r.oid("e1"));
        let rep = run_train(&TrainRequest {
            repo: r.0.clone(),
            onto: "base".into(),
            branches: vec!["e1".into()],
            dry_run: true,
        })
        .expect("実行できる");
        assert!(rep.ok());
        assert!(rep.merged.is_empty(), "乾式なので入れていない");
        assert_eq!((r.oid("base"), r.oid("e1")), before);
    }

    #[test]
    fn 統合先が見つからなければ拒否する() {
        let Some(r) = make_repo("no-onto") else { return };
        let err = run_train(&TrainRequest {
            repo: r.0.clone(),
            onto: "no-such-branch".into(),
            branches: vec!["base".into()],
            dry_run: true,
        })
        .expect_err("拒否する");
        assert!(err.contains("no-such-branch"), "{err}");
    }

    #[test]
    fn コマンドライン入口が計画と実行を通す() {
        let Some(r) = make_repo("cli") else { return };
        r.git(&["checkout", "--quiet", "-b", "f1"]);
        r.commit("solo.txt", "f1\n", "f1");
        r.git(&["checkout", "--quiet", "base"]);
        let repo = r.0.to_string_lossy().to_string();
        let s = |v: &[&str]| -> Vec<String> { v.iter().map(|x| (*x).to_string()).collect() };

        // 使い方の誤りは 2
        assert_eq!(cli_main(&[]), 2);
        assert_eq!(cli_main(&s(&["nope"])), 2);
        assert_eq!(cli_main(&s(&["plan", "--nope"])), 2);
        assert_eq!(cli_main(&s(&["help"])), 0);
        // 計画は参照を動かさない
        let before = (r.oid("base"), r.oid("f1"));
        assert_eq!(
            cli_main(&s(&["plan", "--repo", &repo, "--onto", "base", "--json"])),
            0
        );
        assert_eq!((r.oid("base"), r.oid("f1")), before);
        // 実行は成功して 0
        assert_eq!(cli_main(&s(&["run", "--repo", &repo, "--onto", "base"])), 0);
        assert_eq!(r.git(&["show", "base:solo.txt"]).trim(), "f1");
    }

    #[test]
    fn 衝突で止まったら終了コード1を返す() {
        let Some(r) = make_repo("cli-stop") else { return };
        r.git(&["checkout", "--quiet", "-b", "g1"]);
        r.commit("shared.txt", &edited(20, "g1"), "g1");
        r.git(&["checkout", "--quiet", "base"]);
        r.git(&["checkout", "--quiet", "-b", "g2"]);
        r.commit("shared.txt", &edited(20, "g2"), "g2");
        r.git(&["checkout", "--quiet", "base"]);
        let repo = r.0.to_string_lossy().to_string();
        let argv: Vec<String> = ["run", "--repo", &repo, "--onto", "base"]
            .iter()
            .map(|x| (*x).to_string())
            .collect();
        assert_eq!(cli_main(&argv), 1, "衝突で停止したら 1");
    }
}
