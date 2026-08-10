//! 衝突レーダー — 並列エージェントのマージ衝突を **起きる前** に出す。
//!
//! ## なぜ要るか
//!
//! worktree でエージェントを隔離すると *同時書き込み* は消えるが、
//! *意味的な衝突* は消えない。**マージの瞬間まで先送りされるだけ**である。
//! [`crate::worktree`] の [`build_conflicts`](crate::worktree::build_conflicts)
//! は「同じ作業ツリーに同居している 2 体」だけを見るので、隔離済みの
//! エージェント同士は構造的に 1 件も出ない (そこが隔離の効き目そのもの)。
//! ここはその **裏側** — 隔離された N 本のツリーの間で、後で確実に痛くなる
//! 重なりを、まだ安く軌道修正できるうちに出す層である。
//!
//! ## 段を分けて、いちばん強い段で報告する
//!
//! 1. **ファイル単位** — 2 本のツリーが同じパスを触った。最低ライン。
//!    これだけを鳴らすと `src/app.rs` のような大きな共有ファイルで
//!    狼少年になるので、**この段だけでは警報にしない** ([`Severity::Info`])。
//! 2. **ハンク単位** — 共通ベースに対する変更行範囲が実際に重なるか、
//!    [`NEAR_LINES`] 行以内に近接する。ここが信頼の分かれ目。
//! 3. **実マージ** — `git merge-tree --write-tree` で三方向マージを
//!    **作業ツリーに触れずに** 計算する。git 2.38 未満では使えないので
//!    [`supports_merge_tree`] で判定し、無ければ 2 段目までへ綺麗に降格する。
//!
//! ## 過剰報告より過少報告
//!
//! 誤検知する衝突レーダーは初日にオフにされる。だから
//! **両側が同一の変更をしている場合は衝突として数えない** ([`same_change`])、
//! **merge-tree が「綺麗にマージできる」と言った組は
//! [`Severity::Info`] へ落とす**、という向きに倒してある。
//! バッジが数えるのは [`Severity::Warn`] 以上だけ ([`Report::alarm_files`])。
//!
//! ## UI スレッドで git を待たない
//!
//! 走査は [`ConflictRadar`] が丸ごと裏スレッドへ逃がし、UI へは
//! **いま手元にある結果** を返す (古くてよい)。次の走査までの間隔は
//! [`crate::git::scan_interval`] — 直近の所要時間の 4 倍まで自動で後退するので、
//! 遅いリポジトリで git が常時走り続けることがない。
//! 見張る対象が 2 本未満なら **git を 1 回も起こさない**。
//!
//! ## `gix` を採らなかった理由 (実測に基づく)
//!
//! `gix` はライブラリなのでプロセスを起こさずに N×N のマージ行列を回せるが、
//!
//! * `gix` の既定 feature は `zlib-ng` (C) を引き、`build.rs` とネイティブ
//!   ライブラリを持ち込む。Cargo.toml の既存の判断基準
//!   (「純 Rust で build.rs もネイティブライブラリも要らないか」) に反する。
//! * 三方向マージ (`gix-merge`) まで有効にすると依存が 100 crate 近く増える。
//!   現在のロックは 548 crate で、2 割近い増分になる。
//! * 一方 `git merge-tree` のプロセス生成コストは **このリポジトリ (21.6 万行)
//!   で 1 回 31.8ms** (20 回 0.635 秒の実測)。走査間隔は最短でも
//!   [`SCAN_BASE`] = 8 秒なので、8 組でも稼働率は 3% に届かない。
//!
//! よってサブプロセス版を採る。**測って比べた上での判断**であり、
//! ライブラリ化が要るほどの負荷が出たら差し替えればよい。
//!
//! ## 他モジュールから消費できる形 (依存はしない)
//!
//! 検出結果は **git にも UI にも依存しない純粋なデータ**として出してある。
//! 予防側 (ファイル所有リース等) がそのまま食える:
//!
//! * [`Report::all_owners`] — `ファイルパス → 触っているツリーの表示名`。
//!   **1 本しか触っていないファイルも載る**ので「これから 2 本目になる」を
//!   止める用途に使える。
//! * [`Report::hotspots`] — 2 本以上が触っているファイル (危険度の降順)。
//! * [`build_report`] / [`classify_pair`] — 入力さえ作れば git を 1 回も
//!   起こさずに判定できる純関数。
//!
//! **こちらから他モジュールを参照しない。** このモジュールは単独で完成している。
//!
//! ## 正直な限界
//!
//! * `git merge-tree` が見るのは **コミット済み** の状態だけ。作業ツリーが
//!   汚れている組では 3 段目の判定を「権威」として使えないので、
//!   2 段目の予測をそのまま出す ([`Report::note`] に書く)。
//! * `--write-tree` はマージ結果のツリーを **オブジェクト DB へ書く**。
//!   到達不能オブジェクトなので gc で消えるが、「1 バイトも書かない」ではない。
//!   index・ref・作業ツリーには一切触れない。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Align2, RichText};

use crate::i18n::{tr, trf};
use crate::panels::space;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// 定数
// ---------------------------------------------------------------------------

/// 「近接」と見なす行数。git の既定 diff コンテキストと同じ 3 行。
///
/// 3 行以内に別々の変更が並ぶと、git のマージはコンテキストの取り合いで
/// 実際に衝突しうる。それより離れていれば普通は両方取り込める。
pub const NEAR_LINES: usize = 3;

/// 走査の最短間隔。実際の間隔は [`crate::git::scan_interval`] が
/// 直近の所要時間に応じてここから伸ばす。
pub const SCAN_BASE: Duration = Duration::from_secs(8);

/// ディスク使用量を測り直す間隔。**パネルを開けている間だけ** 効く。
pub const DISK_TTL: Duration = Duration::from_secs(60);

/// ディスク使用量の走査で辿るエントリ数の上限。
///
/// worktree には `target/` が入っていて 10 万エントリを超える。全部数えると
/// 数百 ms かかるので打ち切り、打ち切ったことは表示に `+` で出す
/// (「測っていない数字を測ったふりで出さない」)。
pub const DISK_BUDGET: usize = 30_000;

/// `git merge-tree --write-tree` が入ったバージョン。
pub const MERGE_TREE_SINCE: (u32, u32) = (2, 38);

/// パネルに出す行数の上限 (これを超えた分は「他 N 件」に畳む)。
pub const ROWS_MAX: usize = 40;

// ---------------------------------------------------------------------------
// 純粋ロジック — ベース側の変更行範囲
// ---------------------------------------------------------------------------

/// 共通ベース側の変更行範囲。1 始まり・両端を含む。
///
/// 純粋な挿入 (`@@ -p,0 +c,d @@`) は幅を持たないので `insert = true` にして
/// 「ベース行 `p` の **直後** へ差し込む」を表す。`start == end == p`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    /// 挿入点か (幅ゼロ)。
    pub insert: bool,
}

impl Span {
    /// ベース行 `start..=end` を書き換える範囲。
    pub fn edit(start: usize, end: usize) -> Span {
        Span {
            start,
            end: end.max(start),
            insert: false,
        }
    }

    /// ベース行 `at` の直後への挿入点。
    pub fn insert_at(at: usize) -> Span {
        Span {
            start: at,
            end: at,
            insert: true,
        }
    }

    /// 2 つの範囲が実際に重なるか (= git がまず確実に衝突する)。
    ///
    /// 挿入点どうしは **同じ点のときだけ** 重なる。挿入点と書き換え範囲は、
    /// 挿入点が範囲の中か直前 (`start - 1`) にあるとき重なる —
    /// そこへ差し込むと、相手が消そうとしている行の内側に入るため。
    pub fn overlaps(self, other: Span) -> bool {
        match (self.insert, other.insert) {
            (true, true) => self.start == other.start,
            (true, false) => self.start + 1 >= other.start && self.start <= other.end,
            (false, true) => other.start + 1 >= self.start && other.start <= self.end,
            (false, false) => self.start <= other.end && other.start <= self.end,
        }
    }

    /// 重なっていないときの間隔 (行数)。重なっていれば 0。
    pub fn gap(self, other: Span) -> usize {
        if self.overlaps(other) {
            return 0;
        }
        let (lo, hi) = if self.start <= other.start {
            (self, other)
        } else {
            (other, self)
        };
        hi.start.saturating_sub(lo.end).saturating_sub(1)
    }
}

/// 1 本のツリーが 1 ファイルへ加えた変更の種別。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditKind {
    /// 既存ファイルの書き換え
    Modified,
    /// ベースに無かったファイルの新規作成
    Created,
    /// ベースにあったファイルの削除
    Deleted,
    /// リネーム (`from` はベース側のパス)
    Renamed { from: String },
}

/// 1 本のツリーが 1 ファイルへ加えた変更。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEdit {
    /// 突き合わせ・表示に使うリポジトリ相対パス (`/` 区切り)。
    pub path: String,
    pub kind: EditKind,
    /// ベース側の変更行範囲 (開始位置の昇順)。
    pub spans: Vec<Span>,
    /// 変更内容の指紋。**両側が同一の変更をしている** ときに一致する。
    ///
    /// `None` は **共通ベースが取れず指紋を計算できなかった** ことを表す
    /// (`git merge-base` に失敗し、`git status` からファイル名だけを起こした
    /// フォールバック経路)。ここを `0` のような具体値で埋めると
    /// [`same_change`] が **全ペアで真** になり、[`classify_pair`] が全部
    /// `None` を返して **判定が 1 件も出なくなる**。実際にそうなっていて、
    /// 画面には「ファイル単位までしか判定できません」と出るのに、その
    /// **ファイル単位すら 1 件も出ていなかった**。「知らない」は値ではなく
    /// 型で表す。
    pub digest: Option<u64>,
}

/// 2 つの変更が「同じ結果になる同一の変更」か。
///
/// これを衝突として数えると、同じ指示を撒いた 2 体が同じ修正をしただけで
/// 警報が鳴る。git はこれを綺麗にマージするので、**衝突ではない**。
///
/// **指紋が無い (ベース不明の) 側は「同一」と言えない。** 不明どうしを
/// 同一視すると、ベースが取れなかった走査で衝突が 1 件も出なくなる。
pub fn same_change(a: &FileEdit, b: &FileEdit) -> bool {
    let (Some(da), Some(db)) = (a.digest, b.digest) else {
        return false;
    };
    a.kind == b.kind && da == db && a.spans == b.spans
}

/// 変更の指紋を作る。ハンクの中身 (行種別と本文) だけを見るので、
/// 行番号がずれていても同一の変更なら一致する。
fn digest_of(hunks: &[crate::diff::Hunk]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for hunk in hunks {
        for line in &hunk.lines {
            (line.kind as u8).hash(&mut h);
            line.text.hash(&mut h);
        }
    }
    h.finish()
}

/// パスを突き合わせ用に正規化する (`\` → `/`、`./` を落とす)。
///
/// 大文字小文字は [`crate::worktree::fs_case_insensitive`] に従って畳む —
/// macOS / Windows で `SRC/App.rs` と `src/app.rs` は同じファイルなので、
/// 別物として扱うと衝突を **見落とす**。
pub fn norm_path(p: &str) -> String {
    let s = p.replace('\\', "/");
    let s = s.strip_prefix("./").unwrap_or(&s).to_string();
    if crate::worktree::fs_case_insensitive() {
        s.to_lowercase()
    } else {
        s
    }
}

/// `git diff --unified=0 <base>` の出力から、ツリー 1 本ぶんの変更を起こす。
///
/// 行範囲は **ベース側 (共通祖先)** で取る。2 本のブランチは同じベースから
/// 分かれているので、ベース側で重なっていれば実際にマージで衝突する。
pub fn edits_from_diff(diff_text: &str) -> Vec<FileEdit> {
    let mut out = Vec::new();
    for f in crate::diff::parse_unified(diff_text) {
        let kind = if f.is_rename && f.old_path != f.new_path {
            EditKind::Renamed {
                from: norm_path(&f.old_path),
            }
        } else if f.is_deleted_file() {
            EditKind::Deleted
        } else if f.is_new_file() {
            EditKind::Created
        } else {
            EditKind::Modified
        };
        let path = match &kind {
            EditKind::Deleted => norm_path(&f.old_path),
            _ => norm_path(&f.new_path),
        };
        if path.is_empty() {
            continue;
        }
        let mut spans: Vec<Span> = f
            .hunks
            .iter()
            .map(|h| {
                let removed: Vec<usize> = h
                    .lines
                    .iter()
                    .filter(|l| l.kind == crate::diff::LineKind::Removed)
                    .filter_map(|l| l.old_no)
                    .collect();
                match (removed.first(), removed.last()) {
                    (Some(a), Some(b)) => Span::edit(*a, *b),
                    // 削除行が 1 本も無い = 純粋な挿入。`@@ -p,0 @@` の p の直後。
                    _ => Span::insert_at(h.old_start),
                }
            })
            .collect();
        spans.sort_unstable();
        out.push(FileEdit {
            path,
            kind,
            digest: Some(digest_of(&f.hunks)),
            spans,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// 純粋ロジック — 深刻度の分類
// ---------------------------------------------------------------------------

/// どれくらい危ないか。**バッジが数えるのは [`Severity::Warn`] 以上だけ**。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// 同じファイルだが離れている / git が「マージできる」と言った。
    /// 知っておくと良いだけで、警報にはしない。
    Info,
    /// 近接している / リネームが絡む。手が入る前に見ておきたい。
    Warn,
    /// 範囲が重なる・削除と編集・両側で新規作成。ほぼ確実に衝突する。
    Certain,
}

impl Severity {
    /// 見出しの記号。
    pub fn glyph(self) -> &'static str {
        match self {
            Severity::Certain => "🛑",
            Severity::Warn => "⚠",
            Severity::Info => "•",
        }
    }

    pub fn label(self) -> String {
        match self {
            Severity::Certain => tr("衝突確実"),
            Severity::Warn => tr("要注意"),
            Severity::Info => tr("情報"),
        }
    }

    /// テーマ上の色。
    pub fn color(self, theme: &Theme) -> egui::Color32 {
        match self {
            Severity::Certain => theme.err,
            Severity::Warn => theme.warn,
            Severity::Info => theme.text_dim,
        }
    }
}

/// なぜそう判定したか。**画面へ理由を出す** ので enum で持つ
/// (「なんとなく赤い」を作らない)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// 変更行範囲が重なっている
    Overlap,
    /// 変更行範囲が近い (`NEAR_LINES` 行以内)
    Near,
    /// 同じファイルだが離れている
    SameFile,
    /// 片方が削除、片方が編集
    DeleteEdit,
    /// 両側が同じパスを新規作成
    AddAdd,
    /// リネームが絡む
    Rename,
    /// `git merge-tree` が実際に衝突すると言った
    MergeTree,
    /// `git merge-tree` が綺麗にマージできると言った (降格)
    MergeClean,
    /// 共通ベースが取れず、ファイル単位でしか突き合わせられていない
    BaseUnknown,
}

impl Reason {
    pub fn label(self) -> String {
        match self {
            Reason::Overlap => tr("変更した行が重なっています"),
            Reason::Near => tr("変更した行が近接しています"),
            Reason::SameFile => tr("同じファイルの離れた場所です"),
            Reason::DeleteEdit => tr("片方が削除・片方が編集しています"),
            Reason::AddAdd => tr("両方が同じパスを新規作成しています"),
            Reason::Rename => tr("リネームが絡んでいます"),
            Reason::MergeTree => tr("git が実際に衝突すると判定しました"),
            Reason::MergeClean => tr("git は綺麗にマージできると判定しました"),
            Reason::BaseUnknown => tr("共通ベースが無いためファイル単位でのみ見ています"),
        }
    }
}

/// 2 本のツリーの「同じファイルへの変更」を突き合わせて 1 つの判定にする。
///
/// `None` は **衝突ではない** — 呼び出し側は 1 ピクセルも描かないこと。
pub fn classify_pair(a: &FileEdit, b: &FileEdit, near: usize) -> Option<(Severity, Reason)> {
    // ① 両側が同一の変更 → 衝突ではない (git は綺麗に畳む)。
    if same_change(a, b) {
        return None;
    }
    use EditKind as K;
    match (&a.kind, &b.kind) {
        // ② 両方が消した → 結果は同じ。git は衝突させない。
        (K::Deleted, K::Deleted) => None,
        // ③ 片方が消して片方が生かした → delete/modify。人手が要る。
        (K::Deleted, _) | (_, K::Deleted) => Some((Severity::Certain, Reason::DeleteEdit)),
        // ④ 両側が同じパスを新規作成 (中身は違う) → add/add。
        (K::Created, K::Created) => Some((Severity::Certain, Reason::AddAdd)),
        // ⑤ リネームが絡む。git の追跡がずれるので、行が離れていても見ておく。
        (K::Renamed { .. }, _) | (_, K::Renamed { .. }) => Some((Severity::Warn, Reason::Rename)),
        // ⑥ 片方だけ新規作成 = ベースの見え方が食い違っている。
        (K::Created, _) | (_, K::Created) => Some((Severity::Certain, Reason::AddAdd)),
        // ⑦ 素直な書き換えどうし。ここだけ行範囲で段を分ける。
        (K::Modified, K::Modified) => {
            // **ベース不明 (指紋が無い) 側は行範囲も持っていない。**
            // そのまま行範囲だけを見ると `spans` が空どうしで
            // `SameFile` にすら届かず、**1 件も出ない**まま終わる。
            // ファイル単位の重なりは必ず 1 件出す — ただし段は
            // [`Severity::Info`] に留める (`src/app.rs` のような大きな
            // 共有ファイルで狼少年にならないための、冒頭の約束どおり)。
            if a.digest.is_none() || b.digest.is_none() {
                return Some((Severity::Info, Reason::BaseUnknown));
            }
            let mut best = None;
            for x in &a.spans {
                for y in &b.spans {
                    if x.overlaps(*y) {
                        return Some((Severity::Certain, Reason::Overlap));
                    }
                    if x.gap(*y) <= near {
                        best = Some((Severity::Warn, Reason::Near));
                    }
                }
            }
            Some(best.unwrap_or((Severity::Info, Reason::SameFile)))
        }
    }
}

/// 判定に添える「どのベース行のあたりか」(表示とジャンプ用)。
fn focus_line(a: &FileEdit, b: &FileEdit) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for x in &a.spans {
        for y in &b.spans {
            let g = x.gap(*y);
            let line = x.start.min(y.start).max(1);
            if best.is_none_or(|(bg, _)| g < bg) {
                best = Some((g, line));
            }
        }
    }
    best.map(|(_, l)| l)
        .or_else(|| a.spans.first().or(b.spans.first()).map(|s| s.start.max(1)))
}

// ---------------------------------------------------------------------------
// レポート
// ---------------------------------------------------------------------------

/// 走査対象 1 本ぶんの指定 (UI 側が組み立てる)。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TreeSpec {
    /// エージェントのセッション ID。リポジトリ本体は 0。
    pub id: u64,
    /// 表示名 (エージェント名 / フォルダ名)。
    pub label: String,
    /// ブランチ名。**表示のためだけ** に持つ — 実マージには
    /// `rev-parse HEAD` の OID を渡す (detached でも動き、名前の取り違えも無い)。
    pub branch: String,
    /// 作業ツリーのフォルダ。
    pub dir: PathBuf,
}

/// 走査したツリーの結果 (表示用)。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreeInfo {
    pub id: u64,
    pub label: String,
    pub branch: String,
    pub dir: PathBuf,
    /// 未コミットの変更があるか。**あると `merge-tree` の判定は権威にならない**。
    pub dirty: bool,
    /// ベース以降に触ったファイル (正規化済み相対パス・昇順)。
    /// ディスパッチ前チェックの逆引きに使う。
    pub files: Vec<String>,
    /// `(バイト数, 打ち切ったか)`。測っていなければ `None`。
    pub disk: Option<(u64, bool)>,
}

/// 1 組 × 1 ファイルの判定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub path: String,
    /// [`Report::trees`] の添字 (`a < b`)。
    pub a: usize,
    pub b: usize,
    pub severity: Severity,
    pub reason: Reason,
    /// ベース側のだいたいの行 (エディタで開くときの目印)。
    pub line: Option<usize>,
}

/// 行列 1 マスの内訳。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    pub certain: usize,
    pub warn: usize,
    pub info: usize,
}

impl Cell {
    /// 警報になる件数 (バッジと行列の数字)。
    pub fn alarms(self) -> usize {
        self.certain + self.warn
    }

    pub fn total(self) -> usize {
        self.certain + self.warn + self.info
    }

    /// このマスの代表的な深刻度。
    pub fn worst(self) -> Option<Severity> {
        if self.certain > 0 {
            Some(Severity::Certain)
        } else if self.warn > 0 {
            Some(Severity::Warn)
        } else if self.info > 0 {
            Some(Severity::Info)
        } else {
            None
        }
    }
}

/// **ファイル所有の逆引き** — このファイルを誰が触っているか。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hotspot {
    pub path: String,
    /// 触っているツリーの添字 (昇順)。
    pub trees: Vec<usize>,
    /// このファイルで最も強い判定。誰ともぶつかっていなければ `None`。
    pub worst: Option<Severity>,
}

/// 1 回の走査の結果。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub trees: Vec<TreeInfo>,
    /// 判定 (深刻度の降順 → パス昇順 → 組の順)。
    pub hits: Vec<Hit>,
    /// ファイル → 触っているツリー (衝突の強い順 → 触っている数の多い順)。
    pub hotspots: Vec<Hotspot>,
    /// 使った共通ベース (短縮 OID)。取れなければ `None` = ハンク段は使えない。
    pub base: Option<String>,
    /// 降格したときの理由。画面に **そのまま** 出す。
    pub note: Option<String>,
    /// 走査にかかった時間。次の間隔を決めるのに使う。
    pub took: Duration,
}

impl Report {
    /// 警報になるファイル数 (重複を畳んだ本数)。**0 なら 1 ピクセルも描かない**。
    pub fn alarm_files(&self) -> usize {
        self.hits
            .iter()
            .filter(|h| h.severity >= Severity::Warn)
            .map(|h| h.path.as_str())
            .collect::<BTreeSet<_>>()
            .len()
    }

    /// 警報が 1 件も無いか。
    pub fn is_quiet(&self) -> bool {
        self.alarm_files() == 0
    }

    /// 行列 1 マスの内訳。
    pub fn cell(&self, a: usize, b: usize) -> Cell {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let mut c = Cell::default();
        for h in self.hits.iter().filter(|h| h.a == a && h.b == b) {
            match h.severity {
                Severity::Certain => c.certain += 1,
                Severity::Warn => c.warn += 1,
                Severity::Info => c.info += 1,
            }
        }
        c
    }

    /// カードのツールチップ本文。警報が無ければ `None` (何も描かない)。
    pub fn card_hint(&self, id: u64) -> Option<String> {
        let i = self.trees.iter().position(|t| t.id == id)?;
        let mut who: BTreeMap<usize, BTreeSet<&str>> = BTreeMap::new();
        for h in self.hits.iter().filter(|h| h.severity >= Severity::Warn) {
            let other = match (h.a == i, h.b == i) {
                (true, _) => h.b,
                (_, true) => h.a,
                _ => continue,
            };
            who.entry(other).or_default().insert(&h.path);
        }
        if who.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        for (other, files) in who {
            let name = self
                .trees
                .get(other)
                .map(|t| t.label.clone())
                .unwrap_or_default();
            let head: Vec<&str> = files.iter().take(3).copied().collect();
            let rest = files.len().saturating_sub(head.len());
            let mut s = trf(
                "⚠ {who} と {n} ファイル衝突",
                &[("who", name), ("n", files.len().to_string())],
            );
            s.push_str(&format!("\n  {}", head.join(", ")));
            if rest > 0 {
                s.push_str(&trf(" 他 {n} 件", &[("n", rest.to_string())]));
            }
            lines.push(s);
        }
        Some(lines.join("\n"))
    }

    /// **ファイル所有の逆引き** — ファイルパス → いま触っているツリーの表示名。
    ///
    /// [`Report::hotspots`] と違い **1 本しか触っていないファイルも載る** —
    /// ディスパッチ前チェックは「これから 2 本目になる」のを止めるのが仕事で、
    /// 既に 2 本になってからでは遅いため。`exclude` に渡したセッションは
    /// 外す (自分が既に持っているファイルを自分へ警告しない)。
    pub fn all_owners(&self, exclude: Option<u64>) -> BTreeMap<String, Vec<String>> {
        let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for t in self.trees.iter().filter(|t| Some(t.id) != exclude) {
            for f in &t.files {
                m.entry(f.clone()).or_default().push(t.label.clone());
            }
        }
        m
    }
}

/// 見出しに出す状態。**「✅ 衝突は見つかっていません」を安易に出さない**ための分岐。
///
/// 共通ベースが取れなかった走査は行単位を一度も見ていないので、
/// [`Severity::Warn`] 以上は構造的に 1 件も立たない。そこで
/// [`Report::is_quiet`] だけを見て ✅ を出すと、**何も見ていないのに
/// 「安全」と言う**ことになる。純関数にしてテーブルテストで固定する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Headline {
    /// 見張る対象が 2 本未満 (git を 1 回も起こしていない)
    TooFew,
    /// 行単位まで見たうえで静か
    Quiet,
    /// 共通ベースが無く、ファイル単位の重なりだけが `n` ファイル見えている
    FileLevelOnly(usize),
    /// 警報が `n` ファイル
    Alarm(usize),
}

/// [`Report`] から見出しの状態を決める **純関数**。
pub fn headline(rep: &Report) -> Headline {
    if rep.trees.len() < 2 {
        return Headline::TooFew;
    }
    if !rep.is_quiet() {
        return Headline::Alarm(rep.alarm_files());
    }
    if rep.base.is_none() && !rep.hits.is_empty() {
        let n = rep
            .hits
            .iter()
            .map(|h| h.path.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        return Headline::FileLevelOnly(n);
    }
    Headline::Quiet
}

/// 走査結果から [`Report`] を畳む **純関数**。ここに git は 1 行も無い。
///
/// `edits` は `trees` と同じ並び。`merged_clean` は
/// `git merge-tree` が「綺麗にマージできる」と言った組 (`a < b`)。
/// `merged_conflict` は実際に衝突すると言った `(a, b, パス)`。
pub fn build_report(
    trees: Vec<TreeInfo>,
    edits: &[Vec<FileEdit>],
    merged_clean: &BTreeSet<(usize, usize)>,
    merged_conflict: &BTreeMap<(usize, usize), BTreeSet<String>>,
    near: usize,
) -> Report {
    let mut hits: Vec<Hit> = Vec::new();
    for a in 0..edits.len() {
        for b in (a + 1)..edits.len() {
            let by_path: HashMap<&str, &FileEdit> =
                edits[b].iter().map(|e| (e.path.as_str(), e)).collect();
            for ea in &edits[a] {
                let Some(eb) = by_path.get(ea.path.as_str()) else {
                    continue;
                };
                let Some((mut sev, mut why)) = classify_pair(ea, eb, near) else {
                    continue;
                };
                // ── 3 段目 (実マージ) が権威を持てる組だけ、判定を上書きする ──
                let conflicted = merged_conflict
                    .get(&(a, b))
                    .is_some_and(|s| s.contains(&ea.path));
                if conflicted {
                    sev = Severity::Certain;
                    why = Reason::MergeTree;
                } else if merged_clean.contains(&(a, b)) {
                    // git が「マージできる」と言った = 警報にしない。
                    // 消してしまうと「同じファイルを触っている」事実まで
                    // 見えなくなるので、情報としては残す。
                    sev = Severity::Info;
                    why = Reason::MergeClean;
                }
                hits.push(Hit {
                    path: ea.path.clone(),
                    a,
                    b,
                    severity: sev,
                    reason: why,
                    line: focus_line(ea, eb),
                });
            }
        }
    }
    // merge-tree だけが知っている衝突 (ハンク段が見落とした分) も拾う。
    for ((a, b), paths) in merged_conflict {
        for p in paths {
            if hits.iter().any(|h| h.a == *a && h.b == *b && h.path == *p) {
                continue;
            }
            hits.push(Hit {
                path: p.clone(),
                a: *a,
                b: *b,
                severity: Severity::Certain,
                reason: Reason::MergeTree,
                line: None,
            });
        }
    }
    hits.sort_by(|x, y| {
        y.severity
            .cmp(&x.severity)
            .then_with(|| x.path.cmp(&y.path))
            .then_with(|| (x.a, x.b).cmp(&(y.a, y.b)))
    });

    // ── 逆引き: ファイル → 触っているツリー ──
    let mut by_file: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for (i, es) in edits.iter().enumerate() {
        for e in es {
            by_file.entry(e.path.clone()).or_default().insert(i);
        }
    }
    let mut hotspots: Vec<Hotspot> = by_file
        .into_iter()
        .filter(|(_, who)| who.len() >= 2)
        .map(|(path, who)| {
            let worst = hits
                .iter()
                .filter(|h| h.path == path)
                .map(|h| h.severity)
                .max();
            Hotspot {
                path,
                trees: who.into_iter().collect(),
                worst,
            }
        })
        .collect();
    // 危ないものが上、次に取り合っている人数が多いもの。
    hotspots.sort_by(|x, y| {
        y.worst
            .cmp(&x.worst)
            .then_with(|| y.trees.len().cmp(&x.trees.len()))
            .then_with(|| x.path.cmp(&y.path))
    });

    Report {
        trees,
        hits,
        hotspots,
        ..Report::default()
    }
}

// ---------------------------------------------------------------------------
// `git merge-tree` — 実マージの判定 (純粋部分)
// ---------------------------------------------------------------------------

/// この git が `git merge-tree --write-tree` を持っているか (2.38+)。
/// 判別できなければ **false** — 「推測しない」で 2 段目まで使う。
pub fn supports_merge_tree(version_out: &str) -> bool {
    match crate::git::parse_git_version(version_out) {
        Some(v) => v >= MERGE_TREE_SINCE,
        None => false,
    }
}

/// `git -C <dir>` の後ろに続く引数列。
///
/// `-z` にするのは、空白や日本語や改行を含むパスでもレコードが壊れないため。
pub fn merge_tree_argv(a: &str, b: &str) -> Vec<String> {
    vec![
        "merge-tree".into(),
        "--write-tree".into(),
        "--name-only".into(),
        "-z".into(),
        a.into(),
        b.into(),
    ]
}

/// `git merge-tree --write-tree --name-only -z` の出力を読む。
///
/// 形は `<ツリー OID>\0<衝突パス>\0…\0\0<情報メッセージ>`。
/// **エラーのときも終了コードは 1** (実測: `not something we can merge`) なので、
/// 終了コードでは衝突と失敗を区別できない。先頭が 40 桁の 16 進 OID かどうかで
/// 判定し、そうでなければ `None` = 失敗として扱う (勝手に衝突扱いしない)。
pub fn parse_merge_tree(stdout: &str) -> Option<Vec<String>> {
    let mut it = stdout.split('\0');
    let oid = it.next()?.trim();
    let looks_oid = oid.len() >= 40 && oid.chars().all(|c| c.is_ascii_hexdigit());
    if !looks_oid {
        return None;
    }
    Some(it.take_while(|f| !f.is_empty()).map(norm_path).collect())
}

// ---------------------------------------------------------------------------
// ディスク使用量 (競合が「無い」と公に認めた領域。パネルを開けている間だけ測る)
// ---------------------------------------------------------------------------

/// `dir` 以下のファイルサイズ合計。`budget` エントリで打ち切る。
///
/// 返り値は `(バイト数, 打ち切ったか)`。シンボリックリンクは辿らない
/// (循環で永久に回らないため)。
pub fn dir_size_capped(dir: &Path, budget: usize) -> (u64, bool) {
    let mut total = 0u64;
    let mut seen = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for ent in rd.flatten() {
            seen += 1;
            if seen > budget {
                return (total, true);
            }
            let Ok(md) = ent.metadata() else { continue };
            if md.is_symlink() {
                continue;
            }
            if md.is_dir() {
                stack.push(ent.path());
            } else {
                total += md.len();
            }
        }
    }
    (total, false)
}

/// バイト数を人間向けに畳む。`truncated` なら末尾に `+`。
pub fn human_bytes(bytes: u64, truncated: bool) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    let plus = if truncated { "+" } else { "" };
    if u == 0 {
        format!("{bytes} {}{plus}", UNITS[0])
    } else {
        format!("{v:.1} {}{plus}", UNITS[u])
    }
}

// ---------------------------------------------------------------------------
// ディスパッチ前チェック
// ---------------------------------------------------------------------------

/// ディスパッチを止めるほどではないが、投げる前に見せる警告 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchWarning {
    pub path: String,
    /// 既にそのファイルを触っているツリーの表示名。
    pub owners: Vec<String>,
}

/// 指示文の中の「**既に誰かが触っているパス**」を拾う。
///
/// **推測しない。** 拾うのは `owners` に載っているパスと一致したトークンだけで、
/// 「パスらしい文字列」を自前で判定したりはしない。だから
/// `Read(src/error_handling.rs)` のような文字列を誤って警告に数えない。
/// 一致は完全一致か、`/` 境界で終わる接尾辞 (`./src/app.rs` や
/// 絶対パス表記) のみ — 裸の `app.rs` は拾わない (過少報告に倒す)。
pub fn dispatch_check(text: &str, owners: &BTreeMap<String, Vec<String>>) -> Vec<DispatchWarning> {
    if text.trim().is_empty() || owners.is_empty() {
        return Vec::new();
    }
    // 記号で割ってトークンにする。バッククォート・引用符・括弧・カンマは
    // プロンプトで頻出するので必ず落とす。
    let tokens: BTreeSet<String> = text
        .split(|c: char| c.is_whitespace() || "`'\"()[]{}<>,;:*|".contains(c))
        .filter(|t| !t.is_empty())
        .map(|t| norm_path(t.trim_matches(|c: char| c == '.' || c == '/')))
        .filter(|t| !t.is_empty())
        .collect();
    let mut out = Vec::new();
    for (path, who) in owners {
        let hit = tokens.contains(path)
            || tokens.iter().any(|t| {
                t.len() > path.len() && t.ends_with(path.as_str()) && {
                    let head = &t[..t.len() - path.len()];
                    head.ends_with('/')
                }
            });
        if hit {
            out.push(DispatchWarning {
                path: path.clone(),
                owners: who.clone(),
            });
        }
    }
    out
}

/// 警告をプロンプト送信前の 1 行にまとめる。
pub fn dispatch_summary(warns: &[DispatchWarning]) -> String {
    if warns.is_empty() {
        return String::new();
    }
    let files: Vec<&str> = warns.iter().take(3).map(|w| w.path.as_str()).collect();
    let rest = warns.len().saturating_sub(files.len());
    let mut s = trf(
        "⚠ このファイルは既に別のエージェントが触っています: {files}",
        &[("files", files.join(", "))],
    );
    if rest > 0 {
        s.push_str(&trf(" 他 {n} 件", &[("n", rest.to_string())]));
    }
    s
}

// ---------------------------------------------------------------------------
// レイアウト (純関数・テーブルテストで固定する)
// ---------------------------------------------------------------------------

/// 衝突マトリクスの寸法。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MatrixLayout {
    /// 行見出し (ツリー名) の幅
    pub head_w: f32,
    /// セル 1 個の幅・高さ
    pub cell: f32,
    /// 実際に描く本数 (可用幅・高さに入り切らない分は畳む)
    pub shown: usize,
    /// 畳んだ本数
    pub folded: usize,
    /// 行列を諦めて一覧だけ出すか (幅が足りない)
    pub list_only: bool,
}

/// セルの最小寸法。数字 2 桁 + 余白が読める大きさ。
const CELL_MIN: f32 = 28.0;
/// セルの最大寸法 (これ以上広げても意味が無い)。
const CELL_MAX: f32 = 44.0;
/// 行見出しに要る最小幅。
const HEAD_MIN: f32 = 96.0;
/// 列見出しの高さ。
pub const HEADER_H: f32 = 22.0;

/// 可用領域とツリー本数から行列の寸法を決める **純関数**。
///
/// * 幅が足りなければ `list_only` にして行列を描かない
///   (見切れた行列は読めないので、出さない方がよい)。
/// * 高さが足りなければ後ろを畳む (`folded`)。**はみ出させない**。
pub fn matrix_layout(avail_w: f32, avail_h: f32, n: usize) -> MatrixLayout {
    let mut lay = MatrixLayout {
        head_w: HEAD_MIN,
        cell: CELL_MIN,
        shown: 0,
        folded: n,
        list_only: true,
    };
    if n < 2 {
        return lay;
    }
    // 何本まで横に置けるか。
    let room = (avail_w - HEAD_MIN).max(0.0);
    let fits_w = (room / CELL_MIN).floor().max(0.0) as usize;
    // 縦は見出し 1 行 + 本数ぶん。
    let rows_room = (avail_h - HEADER_H).max(0.0);
    let fits_h = (rows_room / CELL_MIN).floor().max(0.0) as usize;
    let shown = n.min(fits_w).min(fits_h);
    if shown < 2 {
        return lay;
    }
    // セルは**幅と高さの両方**に収まる大きさにする。幅だけで決めると、
    // 横に余裕がある細長い窓 (1200x300) で行が下へはみ出す (実際に出た)。
    let cell = (room / shown as f32)
        .min(rows_room / shown as f32)
        .floor()
        .clamp(CELL_MIN, CELL_MAX);
    lay.head_w = (avail_w - cell * shown as f32).max(HEAD_MIN);
    lay.cell = cell;
    lay.shown = shown;
    lay.folded = n - shown;
    lay.list_only = false;
    lay
}

/// 行列を構成する 1 要素。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// 列見出し (ツリーの添字)
    ColHead(usize),
    /// 行見出し (ツリーの添字)
    RowHead(usize),
    /// マス (行, 列)
    Cell(usize, usize),
}

/// 行列の全要素と、その矩形。**描画とテストが同じ式を通る** ようにここへ
/// 1 本化してある (描画側だけ式が変わって「テストは緑なのに見切れる」を防ぐ)。
pub fn matrix_slots(lay: &MatrixLayout, origin: egui::Pos2) -> Vec<(Slot, egui::Rect)> {
    let mut out = Vec::new();
    if lay.list_only {
        return out;
    }
    for c in 0..lay.shown {
        let x = origin.x + lay.head_w + c as f32 * lay.cell;
        out.push((
            Slot::ColHead(c),
            egui::Rect::from_min_size(egui::pos2(x, origin.y), egui::vec2(lay.cell, HEADER_H)),
        ));
    }
    for r in 0..lay.shown {
        let y = origin.y + HEADER_H + r as f32 * lay.cell;
        out.push((
            Slot::RowHead(r),
            egui::Rect::from_min_size(egui::pos2(origin.x, y), egui::vec2(lay.head_w, lay.cell)),
        ));
        for c in 0..lay.shown {
            let x = origin.x + lay.head_w + c as f32 * lay.cell;
            out.push((
                Slot::Cell(r, c),
                egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(lay.cell, lay.cell)),
            ));
        }
    }
    out
}

/// 行列が占める総寸法 ([`matrix_slots`] と同じ式から導く)。
pub fn matrix_size(lay: &MatrixLayout) -> egui::Vec2 {
    if lay.list_only {
        return egui::Vec2::ZERO;
    }
    egui::vec2(
        lay.head_w + lay.cell * lay.shown as f32,
        HEADER_H + lay.cell * lay.shown as f32,
    )
}

// ---------------------------------------------------------------------------
// 走査 (裏スレッド)
// ---------------------------------------------------------------------------

/// 走査 1 回ぶんの仕事。**必ずワーカースレッドから呼ぶこと。**
///
/// git の呼び出し回数は `1 (バージョン) + 1 (共通ベース) + 3N (HEAD / diff /
/// status) + P (ファイル単位で重なった組だけ merge-tree)`。
/// `P` を全組ではなく重なった組に絞るのが「安い段で足切りする」の実装。
pub fn scan(specs: &[TreeSpec], want_disk: bool) -> Report {
    let started = Instant::now();
    let mut report = Report::default();
    if specs.len() < 2 {
        return report;
    }
    let hub = &specs[0].dir;

    // ① 各ツリーの HEAD。merge-tree にはブランチ名ではなく OID を渡す
    //    (detached でも動くうえ、名前の取り違えが起きない)。
    let heads: Vec<Option<String>> = specs
        .iter()
        .map(|s| crate::worktree::git_out(&s.dir, &["rev-parse", "HEAD"]).ok())
        .collect();

    // ② 共通ベース。octopus で 1 回に畳む (組ごとに撃つとプロセスが N² 本になる)。
    let mut base: Option<String> = None;
    let live: Vec<&str> = heads.iter().filter_map(|h| h.as_deref()).collect();
    if live.len() == specs.len() {
        let mut args: Vec<&str> = vec!["merge-base", "--octopus"];
        args.extend_from_slice(&live);
        base = crate::worktree::git_out(hub, &args)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
    }

    // ③ ツリーごとに「ベースからの差分」と「未コミットの有無」。
    //    `git diff <base>` は **作業ツリー** を相手にするので、コミット済みと
    //    未コミットの両方が 1 回で入る (エージェントは大抵コミットしていない)。
    let mut edits: Vec<Vec<FileEdit>> = Vec::with_capacity(specs.len());
    let mut infos: Vec<TreeInfo> = Vec::with_capacity(specs.len());
    for (i, s) in specs.iter().enumerate() {
        let status = crate::worktree::git_out(&s.dir, &["status", "--porcelain=v1", "-z"])
            .unwrap_or_default();
        let dirty = !status.trim().is_empty();
        let mut es = match &base {
            Some(b) => crate::worktree::git_out(
                &s.dir,
                &[
                    "diff",
                    "--unified=0",
                    "--no-color",
                    "--no-ext-diff",
                    "--find-renames",
                    b,
                ],
            )
            .map(|d| edits_from_diff(&d))
            .unwrap_or_default(),
            // ベースが取れないときはファイル単位まで降格する。
            // **指紋は `None`** — ここを 0 で埋めると `same_change` が
            // 全ペアで真になり、判定が 1 件も出なくなる (実際に起きた)。
            None => crate::worktree::status_entries(&s.dir)
                .unwrap_or_default()
                .into_iter()
                .flat_map(|e| e.paths)
                .map(|p| FileEdit {
                    path: norm_path(&p.to_string_lossy()),
                    kind: EditKind::Modified,
                    spans: Vec::new(),
                    digest: None,
                })
                .collect(),
        };
        // 未追跡ファイル (diff には出ない) を新規作成として足す。
        for ent in crate::worktree::parse_status_z(&status) {
            if !ent.is_untracked() {
                continue;
            }
            for p in ent.paths {
                let path = norm_path(&p.to_string_lossy());
                if path.is_empty() || es.iter().any(|e| e.path == path) {
                    continue;
                }
                es.push(FileEdit {
                    path,
                    kind: EditKind::Created,
                    spans: vec![Span::insert_at(0)],
                    // 未追跡ファイルの中身は読まない (大きいかもしれない)。
                    // 指紋を分けておかないと「同一の変更」に誤判定する。
                    digest: Some(i as u64 + 1),
                });
            }
        }
        let mut files: Vec<String> = es.iter().map(|e| e.path.clone()).collect();
        files.sort();
        files.dedup();
        infos.push(TreeInfo {
            id: s.id,
            label: s.label.clone(),
            branch: s.branch.clone(),
            dir: s.dir.clone(),
            dirty,
            files,
            disk: want_disk.then(|| dir_size_capped(&s.dir, DISK_BUDGET)),
        });
        edits.push(es);
    }

    // ④ ファイル単位で重なった組だけ、実マージを撃つ。
    let version = crate::worktree::git_out(hub, &["--version"]).unwrap_or_default();
    let can_merge_tree = supports_merge_tree(&version);
    let mut merged_clean: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut merged_conflict: BTreeMap<(usize, usize), BTreeSet<String>> = BTreeMap::new();
    let mut degraded: Option<String> = None;
    if !can_merge_tree {
        degraded = Some(trf(
            "git {v} には merge-tree --write-tree がありません ({need} 以上が必要)。行範囲だけで判定しています。",
            &[
                ("v", version.trim().to_string()),
                ("need", format!("{}.{}", MERGE_TREE_SINCE.0, MERGE_TREE_SINCE.1)),
            ],
        ));
    } else {
        let mut dirty_pairs = 0usize;
        for a in 0..specs.len() {
            for b in (a + 1)..specs.len() {
                let shares = edits[a]
                    .iter()
                    .any(|x| edits[b].iter().any(|y| y.path == x.path));
                if !shares {
                    continue;
                }
                // 未コミットがある側は merge-tree から見えない。判定を
                // 「権威」として使えないので、2 段目の予測をそのまま残す。
                if infos[a].dirty || infos[b].dirty {
                    dirty_pairs += 1;
                    continue;
                }
                let (Some(ha), Some(hb)) = (&heads[a], &heads[b]) else {
                    continue;
                };
                let args = merge_tree_argv(ha, hb);
                let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                let Some(out) = run_raw(hub, &argv) else {
                    continue;
                };
                match parse_merge_tree(&out) {
                    Some(paths) if paths.is_empty() => {
                        merged_clean.insert((a, b));
                    }
                    Some(paths) => {
                        merged_conflict.insert((a, b), paths.into_iter().collect());
                    }
                    // 出力が読めない = 失敗。勝手に衝突扱いしない。
                    None => {}
                }
            }
        }
        if dirty_pairs > 0 {
            degraded = Some(trf(
                "{n} 組は未コミットの変更があるため、git の実マージ判定を使えません (行範囲からの予測です)。",
                &[("n", dirty_pairs.to_string())],
            ));
        }
    }

    report = build_report(infos, &edits, &merged_clean, &merged_conflict, NEAR_LINES);
    report.base = base.map(|b| b.chars().take(8).collect());
    if report.base.is_none() {
        degraded = Some(tr(
            "共通ベースを特定できないため、ファイル単位までしか判定できません。",
        ));
    }
    report.note = degraded;
    report.took = started.elapsed();
    report
}

/// 終了コードを見ずに stdout をそのまま取る git 実行。
///
/// [`crate::worktree::git_out`] は終了コード != 0 を `Err` にするが、
/// `merge-tree` は **衝突したときも 1 を返す** ので使えない。
fn run_raw(dir: &Path, args: &[&str]) -> Option<String> {
    let out = crate::procx::hidden_command("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    Some(crate::textenc::decode_output(&out.stdout))
}

/// 衝突レーダーの本体。**UI からはこれだけを触る。**
///
/// - 見張る対象が 2 本未満なら git を 1 回も起こさない (アイドルのコストはゼロ)。
/// - 走査は裏スレッド。UI は `try_recv` するだけで待たない。
/// - 次の走査までの間隔は [`crate::git::scan_interval`] が直近の所要時間から決める。
/// - 自分から `request_repaint` を呼ばない (常時アニメーションを作らない)。
#[derive(Default)]
pub struct ConflictRadar {
    report: Arc<Report>,
    rx: Option<Receiver<Report>>,
    watched: Vec<TreeSpec>,
    last: Option<Instant>,
    cost: Option<Duration>,
    /// 直近にディスク使用量を測った時刻。
    disk_at: Option<Instant>,
}

impl ConflictRadar {
    pub fn new() -> Self {
        Self::default()
    }

    /// 1 フレーム進める。`want_disk` はパネルが開いているときだけ true。
    ///
    /// 返り値は「レポートが差し替わったか」。**ここから再描画は要求しない** —
    /// エージェントが動いていれば PTY 出力で描き直しが起きるし、全員止まって
    /// いれば描き直す理由が無い (アイドルのコストはゼロ)。
    pub fn update(&mut self, specs: &[TreeSpec], want_disk: bool) -> bool {
        // 同じフォルダを 2 度数えない (同居エージェントは worktree.rs の担当)。
        let mut uniq: Vec<TreeSpec> = Vec::new();
        for s in specs {
            let key = crate::worktree::path_key(&s.dir);
            if uniq
                .iter()
                .any(|u| crate::worktree::path_key(&u.dir) == key)
            {
                continue;
            }
            uniq.push(s.clone());
        }
        if uniq.len() < 2 {
            let had = !self.watched.is_empty() || !self.report.trees.is_empty();
            self.watched.clear();
            self.rx = None;
            self.last = None;
            self.disk_at = None;
            if had {
                self.report = Arc::new(Report::default());
            }
            return had;
        }
        let changed = uniq != self.watched;
        if changed {
            self.watched = uniq;
        }
        let mut fresh = false;
        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(rep) => {
                    self.cost = Some(rep.took);
                    self.report = Arc::new(rep);
                    self.rx = None;
                    fresh = true;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.rx = None,
            }
        }
        let disk_due = want_disk && self.disk_at.is_none_or(|t| t.elapsed() >= DISK_TTL);
        let wait = crate::git::scan_interval(SCAN_BASE, self.cost);
        let due = changed || disk_due || self.last.is_none_or(|t| t.elapsed() >= wait);
        if self.rx.is_none() && due {
            self.last = Some(Instant::now());
            if disk_due {
                self.disk_at = Some(Instant::now());
            }
            let specs = self.watched.clone();
            let (tx, rx) = channel();
            let spawned = std::thread::Builder::new()
                .name("zv-conflict-radar".into())
                .spawn(move || {
                    let _ = tx.send(scan(&specs, disk_due));
                });
            self.rx = spawned.ok().map(|_| rx);
        }
        fresh
    }

    /// いまのレポート (古くてよい)。
    pub fn report(&self) -> &Report {
        &self.report
    }

    /// 走査中か (スピナーの判断に使う)。
    pub fn scanning(&self) -> bool {
        self.rx.is_some()
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

/// パネルから返る操作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RadarAction {
    /// ファイルを開く (`dir` のツリーの中の相対パス, ベース側の行)。
    Open(PathBuf, usize),
    /// パネルを閉じる
    Close,
}

/// ツリー名を列見出し用に詰める (2〜3 文字)。
fn short_label(label: &str) -> String {
    let t: String = label
        .chars()
        .filter(|c| !c.is_whitespace())
        .take(2)
        .collect();
    if t.is_empty() {
        "?".into()
    } else {
        t
    }
}

/// 衝突マトリクスのウィンドウ。`open` が false なら 1 ピクセルも描かない。
pub fn radar_window(
    ctx: &egui::Context,
    theme: &Theme,
    open: &mut bool,
    radar: &ConflictRadar,
    selected: &mut Option<(usize, usize)>,
) -> Vec<RadarAction> {
    let mut acts = Vec::new();
    if !*open {
        return acts;
    }
    let rep = radar.report();
    let mut win_open = true;
    // **中身が無いときに大きな空箱を開かない。** 見張る相手が 2 本未満なら
    // 高さを指定せず、egui に中身ぶんだけ縮めさせる (「空白は作らない」)。
    let empty = rep.trees.len() < 2;
    let mut win = egui::Window::new(tr("🛰 衝突レーダー"))
        .open(&mut win_open)
        .collapsible(false)
        .resizable(!empty)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0]);
    win = if empty {
        win.default_width(380.0)
    } else {
        win.default_width(620.0).default_height(460.0)
    };
    win.show(ctx, |ui| {
        radar_body(ui, theme, radar, rep, selected, &mut acts);
    });
    if !win_open {
        *open = false;
        acts.push(RadarAction::Close);
    }
    acts
}

fn radar_body(
    ui: &mut egui::Ui,
    theme: &Theme,
    radar: &ConflictRadar,
    rep: &Report,
    selected: &mut Option<(usize, usize)>,
    acts: &mut Vec<RadarAction>,
) {
    // ── 見出し ──
    ui.horizontal(|ui| {
        let (txt, col) = match headline(rep) {
            Headline::TooFew => (
                tr("並列で動いているワークツリーが 1 本以下です"),
                theme.text_dim,
            ),
            Headline::Quiet => (tr("✅ 衝突は見つかっていません"), theme.ok),
            // ベース不明のときに ✅ を出さない。行単位を一度も見ていない。
            Headline::FileLevelOnly(n) => (
                trf(
                    "ℹ {n} ファイルを複数のツリーが触っています (共通ベースが無いため行単位は見ていません)",
                    &[("n", n.to_string())],
                ),
                theme.text_dim,
            ),
            Headline::Alarm(n) => (
                trf("⚠ {n} ファイルが衝突しそうです", &[("n", n.to_string())]),
                theme.warn,
            ),
        };
        ui.label(RichText::new(txt).color(col).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if radar.scanning() {
                ui.label(RichText::new(tr("走査中…")).small().color(theme.text_dim));
            } else if let Some(b) = &rep.base {
                ui.label(
                    RichText::new(trf("ベース {b}", &[("b", b.clone())]))
                        .small()
                        .monospace()
                        .color(theme.text_dim),
                )
                .on_hover_text(tr(
                    "全ワークツリーの共通祖先。ここからの差分で行範囲を突き合わせています。",
                ));
            }
        });
    });
    if let Some(note) = &rep.note {
        ui.label(
            egui::RichText::new(format!("ℹ {note}"))
                .small()
                .color(theme.text_dim),
        );
    }
    if rep.trees.len() < 2 {
        // 空状態は **1 行の案内だけ**。ここで大きなカードを描くと、
        // 「中身より空状態を見せている時間の方が長いパネル」になる。
        ui.label(
            RichText::new(tr(
                "🌿 worktree 隔離でエージェントを 2 体以上動かすと、マージ前に衝突を予測します",
            ))
            .small()
            .color(theme.text_dim),
        );
        return;
    }
    ui.add_space(space::SM);

    // ── 行列 ──
    let lay = matrix_layout(ui.available_width(), 220.0, rep.trees.len());
    if !lay.list_only {
        draw_matrix(ui, theme, rep, &lay, selected);
        if lay.folded > 0 {
            ui.label(
                RichText::new(trf(
                    "… 他 {n} 本は幅が足りないため下の一覧だけに出しています",
                    &[("n", lay.folded.to_string())],
                ))
                .small()
                .color(theme.text_dim),
            );
        }
        ui.add_space(space::SM);
    }

    // ── 選ばれた組 / 全件の一覧 ──
    let rows: Vec<&Hit> = rep
        .hits
        .iter()
        .filter(|h| selected.is_none_or(|(a, b)| h.a == a && h.b == b))
        .take(ROWS_MAX)
        .collect();
    if let Some((a, b)) = *selected {
        ui.horizontal(|ui| {
            let na = rep
                .trees
                .get(a)
                .map(|t| t.label.clone())
                .unwrap_or_default();
            let nb = rep
                .trees
                .get(b)
                .map(|t| t.label.clone())
                .unwrap_or_default();
            ui.label(
                RichText::new(format!("{na} ⇄ {nb}"))
                    .strong()
                    .color(theme.text),
            );
            if ui.small_button(tr("全部見る")).clicked() {
                *selected = None;
            }
        });
    }
    if rows.is_empty() {
        // 空状態は「高さを取らない 1 行」。大きな空カードで場所を潰さない。
        ui.label(
            RichText::new(tr("この組で取り合っているファイルはありません"))
                .small()
                .color(theme.text_dim),
        );
    } else {
        egui::ScrollArea::vertical()
            .id_salt("zv-conflict-hits")
            .max_height(200.0)
            .show(ui, |ui| {
                for h in rows {
                    hit_row(ui, theme, rep, h, acts);
                }
            });
    }

    // ── ホットスポット (ファイル所有の逆引き) ──
    let hot: Vec<&Hotspot> = rep.hotspots.iter().take(8).collect();
    if !hot.is_empty() {
        ui.add_space(space::SM);
        ui.label(
            RichText::new(tr("🔥 取り合いの多いファイル"))
                .small()
                .strong()
                .color(theme.text_dim),
        );
        for h in hot {
            let who: Vec<String> = h
                .trees
                .iter()
                .filter_map(|i| rep.trees.get(*i).map(|t| t.label.clone()))
                .collect();
            let line = format!("{} — {}", h.path, who.join(" ・ "));
            ui.add(
                egui::Label::new(
                    RichText::new(&line)
                        .small()
                        .color(h.worst.map_or(theme.text_dim, |s| s.color(theme))),
                )
                .truncate(),
            )
            .on_hover_text(line);
        }
    }

    // ── ツリーごとの姿 (ディスク使用量つき) ──
    ui.add_space(space::SM);
    ui.separator();
    for t in &rep.trees {
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(RichText::new(&t.label).small().color(theme.text)).truncate());
            let mut tail = trf("{n} ファイル変更", &[("n", t.files.len().to_string())]);
            if t.dirty {
                tail.push_str(&tr(" ・未コミットあり"));
            }
            if let Some((b, cut)) = t.disk {
                tail.push_str(&format!(" ・{}", human_bytes(b, cut)));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(
                    egui::Label::new(RichText::new(tail).small().color(theme.text_dim)).truncate(),
                )
                .on_hover_text(t.dir.display().to_string());
            });
        });
    }
}

fn hit_row(ui: &mut egui::Ui, theme: &Theme, rep: &Report, h: &Hit, acts: &mut Vec<RadarAction>) {
    let na = rep
        .trees
        .get(h.a)
        .map(|t| t.label.clone())
        .unwrap_or_default();
    let nb = rep
        .trees
        .get(h.b)
        .map(|t| t.label.clone())
        .unwrap_or_default();
    let tip = format!(
        "{} {}\n{}\n{na} ⇄ {nb}\n{}",
        h.severity.glyph(),
        h.severity.label(),
        h.path,
        h.reason.label()
    );
    ui.horizontal(|ui| {
        ui.set_width(ui.available_width());
        ui.label(
            RichText::new(h.severity.glyph())
                .color(h.severity.color(theme))
                .small(),
        );
        let label = match h.line {
            Some(l) => format!("{}:{l}", h.path),
            None => h.path.clone(),
        };
        let resp = ui
            .add(
                egui::Label::new(RichText::new(label).monospace().small())
                    .truncate()
                    .sense(egui::Sense::click()),
            )
            .on_hover_text(&tip);
        if resp.clicked() {
            // 開くのは「a 側」のツリー。実際に手を入れるのは大抵こちら。
            if let Some(t) = rep.trees.get(h.a) {
                acts.push(RadarAction::Open(t.dir.join(&h.path), h.line.unwrap_or(1)));
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(format!("{na} ⇄ {nb}"))
                        .small()
                        .color(theme.text_dim),
                )
                .truncate(),
            )
            .on_hover_text(tip);
        });
    });
}

fn draw_matrix(
    ui: &mut egui::Ui,
    theme: &Theme,
    rep: &Report,
    lay: &MatrixLayout,
    selected: &mut Option<(usize, usize)>,
) {
    // 場所は [`matrix_slots`] が決める。描画側で座標を組み直さないので、
    // 「テーブルテストは緑なのに実画面では見切れる」が起こり得ない。
    let (area, _) = ui.allocate_exact_size(matrix_size(lay), egui::Sense::hover());
    let painter = ui.painter().clone();
    let name_of = |i: usize| {
        rep.trees
            .get(i)
            .map(|t| t.label.clone())
            .unwrap_or_default()
    };
    for (slot, rect) in matrix_slots(lay, area.min) {
        match slot {
            Slot::ColHead(c) => {
                painter.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    short_label(&name_of(c)),
                    egui::FontId::proportional(11.0),
                    theme.text_dim,
                );
            }
            Slot::RowHead(r) => {
                painter.text(
                    egui::pos2(rect.left() + space::XS, rect.center().y),
                    Align2::LEFT_CENTER,
                    crate::mcp::ellipsize(&name_of(r), ((lay.head_w - space::SM) / 7.0) as usize),
                    egui::FontId::proportional(12.0),
                    theme.text,
                );
            }
            Slot::Cell(r, c) => {
                if r == c {
                    painter.rect_filled(rect.shrink(1.0), 2.0, theme.panel_alt);
                    continue;
                }
                let cell = rep.cell(r, c);
                let (bg, fg) = match cell.worst() {
                    Some(Severity::Certain) => (theme.err.gamma_multiply(0.30), theme.err),
                    Some(Severity::Warn) => (theme.warn.gamma_multiply(0.25), theme.warn),
                    _ => (theme.panel_alt, theme.text_dim),
                };
                painter.rect_filled(rect.shrink(1.0), 2.0, bg);
                let n = cell.alarms();
                let text = if n > 0 {
                    n.to_string()
                } else if cell.info > 0 {
                    "·".to_string()
                } else {
                    String::new()
                };
                if !text.is_empty() {
                    painter.text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        text,
                        egui::FontId::proportional(12.0),
                        fg,
                    );
                }
                if cell.total() == 0 {
                    continue;
                }
                // ID は座標から作る (`make_persistent_id` を通さないので、
                // 可変長のマスが並んでも衝突しない)。
                let resp = ui.interact(
                    rect,
                    egui::Id::new(("zv-conflict-cell", r, c)),
                    egui::Sense::click(),
                );
                if resp
                    .on_hover_text(trf(
                        "{a} ⇄ {b}: 衝突確実 {c} / 要注意 {w} / 情報 {i}",
                        &[
                            ("a", name_of(r)),
                            ("b", name_of(c)),
                            ("c", cell.certain.to_string()),
                            ("w", cell.warn.to_string()),
                            ("i", cell.info.to_string()),
                        ],
                    ))
                    .clicked()
                {
                    *selected = Some(if r < c { (r, c) } else { (c, r) });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modified(path: &str, spans: &[(usize, usize)], digest: u64) -> FileEdit {
        FileEdit {
            path: path.into(),
            kind: EditKind::Modified,
            spans: spans.iter().map(|(a, b)| Span::edit(*a, *b)).collect(),
            digest: Some(digest),
        }
    }

    /// ベースが取れなかった走査が作る形 (指紋も行範囲も無い)。
    fn base_unknown(path: &str) -> FileEdit {
        FileEdit {
            path: path.into(),
            kind: EditKind::Modified,
            spans: Vec::new(),
            digest: None,
        }
    }

    // -----------------------------------------------------------------
    // 段の分類 (テーブルテスト)
    // -----------------------------------------------------------------

    #[test]
    fn 重なる範囲は衝突確実で離れていれば情報どまり() {
        // (a の範囲, b の範囲, 期待)
        let cases: &[(&[(usize, usize)], &[(usize, usize)], Option<Severity>)] = &[
            // 完全に重なる
            (&[(10, 20)], &[(15, 25)], Some(Severity::Certain)),
            // 端が 1 行だけ重なる
            (&[(10, 20)], &[(20, 30)], Some(Severity::Certain)),
            // 隣接するが重ならない (1 行あき) → 近接
            (&[(10, 20)], &[(22, 30)], Some(Severity::Warn)),
            // ちょうど NEAR_LINES 行あき → 近接
            (&[(10, 20)], &[(24, 30)], Some(Severity::Warn)),
            // NEAR_LINES + 1 行あき → 情報
            (&[(10, 20)], &[(25, 30)], Some(Severity::Info)),
            // 遠い
            (&[(1, 2)], &[(900, 950)], Some(Severity::Info)),
            // 複数ハンクのうち 1 組だけ重なる → 強い方を採る
            (
                &[(1, 2), (100, 110)],
                &[(105, 106), (500, 501)],
                Some(Severity::Certain),
            ),
        ];
        for (sa, sb, want) in cases {
            let a = modified("src/app.rs", sa, 1);
            let b = modified("src/app.rs", sb, 2);
            let got = classify_pair(&a, &b, NEAR_LINES).map(|(s, _)| s);
            assert_eq!(got, *want, "a={sa:?} b={sb:?}");
            // 対称であること (順序を入れ替えても同じ判定)
            let rev = classify_pair(&b, &a, NEAR_LINES).map(|(s, _)| s);
            assert_eq!(rev, *want, "対称でない: a={sa:?} b={sb:?}");
        }
    }

    #[test]
    fn 両側が同一の変更なら衝突ではない() {
        let a = modified("src/app.rs", &[(10, 20)], 777);
        let b = modified("src/app.rs", &[(10, 20)], 777);
        assert_eq!(classify_pair(&a, &b, NEAR_LINES), None);
        // 中身が違えば重なりとして出る
        let c = modified("src/app.rs", &[(10, 20)], 778);
        assert_eq!(
            classify_pair(&a, &c, NEAR_LINES).map(|(s, _)| s),
            Some(Severity::Certain)
        );
    }

    #[test]
    fn 削除と編集は衝突確実で両方削除なら衝突ではない() {
        let del = FileEdit {
            path: "src/old.rs".into(),
            kind: EditKind::Deleted,
            spans: vec![Span::edit(1, 40)],
            digest: Some(1),
        };
        let del2 = FileEdit {
            digest: Some(2),
            ..del.clone()
        };
        let edit = modified("src/old.rs", &[(5, 6)], 3);
        assert_eq!(classify_pair(&del, &del2, NEAR_LINES), None, "両方削除");
        assert_eq!(
            classify_pair(&del, &edit, NEAR_LINES).map(|(s, r)| (s, r)),
            Some((Severity::Certain, Reason::DeleteEdit))
        );
        assert_eq!(
            classify_pair(&edit, &del, NEAR_LINES).map(|(s, _)| s),
            Some(Severity::Certain),
            "順序を入れ替えても同じ"
        );
    }

    #[test]
    fn リネームは行が離れていても要注意() {
        let ren = FileEdit {
            path: "src/new.rs".into(),
            kind: EditKind::Renamed {
                from: "src/old.rs".into(),
            },
            spans: vec![Span::edit(1, 1)],
            digest: Some(1),
        };
        let far = modified("src/new.rs", &[(900, 901)], 2);
        assert_eq!(
            classify_pair(&ren, &far, NEAR_LINES).map(|(s, r)| (s, r)),
            Some((Severity::Warn, Reason::Rename))
        );
    }

    #[test]
    fn 両側が同じパスを新規作成したら衝突確実() {
        let mk = |d: u64| FileEdit {
            path: "docs/plan.md".into(),
            kind: EditKind::Created,
            spans: vec![Span::insert_at(0)],
            digest: Some(d),
        };
        assert_eq!(
            classify_pair(&mk(1), &mk(2), NEAR_LINES).map(|(s, r)| (s, r)),
            Some((Severity::Certain, Reason::AddAdd))
        );
        // 完全に同じものを作ったなら衝突ではない
        assert_eq!(classify_pair(&mk(9), &mk(9), NEAR_LINES), None);
    }

    #[test]
    fn 挿入点は同じ位置のときだけ重なる() {
        // 同じ点への挿入でも**中身が違う**ことを指紋で表す
        // (同じ中身なら `same_change` が先に弾く = 衝突ではない)。
        let ins = |at: usize, d: u64| FileEdit {
            path: "a.txt".into(),
            kind: EditKind::Modified,
            spans: vec![Span::insert_at(at)],
            digest: Some(d),
        };
        assert_eq!(
            classify_pair(&ins(10, 1), &ins(10, 2), NEAR_LINES).map(|(s, _)| s),
            Some(Severity::Certain)
        );
        assert_eq!(
            classify_pair(&ins(10, 1), &ins(10, 1), NEAR_LINES),
            None,
            "同じ点へ同じものを挿す = 同一の変更"
        );
        assert_eq!(
            classify_pair(&ins(10, 1), &ins(11, 2), NEAR_LINES).map(|(s, _)| s),
            Some(Severity::Warn)
        );
        assert_eq!(
            classify_pair(&ins(10, 1), &ins(99, 2), NEAR_LINES).map(|(s, _)| s),
            Some(Severity::Info)
        );
        // 挿入点が相手の書き換え範囲の中 → 重なる
        let m = modified("a.txt", &[(8, 12)], 5);
        assert_eq!(
            classify_pair(&ins(10, 1), &m, NEAR_LINES).map(|(s, _)| s),
            Some(Severity::Certain)
        );
    }

    #[test]
    fn 三体が同じファイルを触ると三組すべてに出る() {
        let edits = vec![
            vec![modified("src/app.rs", &[(10, 12)], 1)],
            vec![modified("src/app.rs", &[(11, 13)], 2)],
            vec![modified("src/app.rs", &[(900, 901)], 3)],
        ];
        let trees: Vec<TreeInfo> = (0..3)
            .map(|i| TreeInfo {
                id: i as u64,
                label: format!("w{i}"),
                ..TreeInfo::default()
            })
            .collect();
        let rep = build_report(
            trees,
            &edits,
            &BTreeSet::new(),
            &BTreeMap::new(),
            NEAR_LINES,
        );
        assert_eq!(rep.hits.len(), 3, "3 組すべてが出る");
        assert_eq!(rep.cell(0, 1).certain, 1);
        assert_eq!(rep.cell(0, 2).info, 1);
        assert_eq!(rep.cell(1, 2).info, 1);
        // 警報になるのは重なった 1 ファイルだけ (離れている組は数えない)
        assert_eq!(rep.alarm_files(), 1);
        // 逆引き: 3 体が触っている
        assert_eq!(rep.hotspots.len(), 1);
        assert_eq!(rep.hotspots[0].trees, vec![0, 1, 2]);
    }

    #[test]
    fn mergetreeが綺麗と言った組は情報へ降格する() {
        let edits = vec![
            vec![modified("src/app.rs", &[(10, 12)], 1)],
            vec![modified("src/app.rs", &[(11, 13)], 2)],
        ];
        let trees: Vec<TreeInfo> = (0..2)
            .map(|i| TreeInfo {
                id: i as u64,
                label: format!("w{i}"),
                ..TreeInfo::default()
            })
            .collect();
        let mut clean = BTreeSet::new();
        clean.insert((0usize, 1usize));
        let rep = build_report(trees.clone(), &edits, &clean, &BTreeMap::new(), NEAR_LINES);
        assert_eq!(rep.hits[0].severity, Severity::Info);
        assert_eq!(rep.hits[0].reason, Reason::MergeClean);
        assert!(rep.is_quiet(), "git が綺麗と言ったなら警報を鳴らさない");

        // 逆に衝突すると言われたら、離れていても衝突確実へ上がる
        let far = vec![
            vec![modified("src/app.rs", &[(1, 2)], 1)],
            vec![modified("src/app.rs", &[(900, 901)], 2)],
        ];
        let mut conf: BTreeMap<(usize, usize), BTreeSet<String>> = BTreeMap::new();
        conf.insert((0, 1), ["src/app.rs".to_string()].into_iter().collect());
        let rep2 = build_report(trees, &far, &BTreeSet::new(), &conf, NEAR_LINES);
        assert_eq!(rep2.hits[0].severity, Severity::Certain);
        assert_eq!(rep2.hits[0].reason, Reason::MergeTree);
    }

    #[test]
    fn 触っているファイルが被らなければ何も出ない() {
        let edits = vec![
            vec![modified("src/a.rs", &[(1, 5)], 1)],
            vec![modified("src/b.rs", &[(1, 5)], 2)],
        ];
        let trees: Vec<TreeInfo> = (0..2).map(|_| TreeInfo::default()).collect();
        let rep = build_report(
            trees,
            &edits,
            &BTreeSet::new(),
            &BTreeMap::new(),
            NEAR_LINES,
        );
        assert!(rep.hits.is_empty());
        assert!(rep.hotspots.is_empty());
        assert!(rep.is_quiet());
    }

    /// **回帰**: 共通ベースが取れなかった走査で、判定が 1 件も出なくなっていた。
    ///
    /// フォールバック ([`scan`]) は全 [`FileEdit`] を `Modified` / `spans` 空 /
    /// 指紋 0 で作っていたので、[`same_change`] が全ペアで真になり
    /// [`classify_pair`] が全部 `None` を返していた。画面には
    /// 「ファイル単位までしか判定できません」と出るのに、その
    /// **ファイル単位すら 1 件も出ない**という、注記そのものが嘘になる状態。
    #[test]
    fn ベース不明でもファイル単位の重なりは必ず出る() {
        let a = base_unknown("src/app.rs");
        let b = base_unknown("src/app.rs");
        assert!(
            !same_change(&a, &b),
            "指紋の無い側どうしを「同一の変更」と言わない"
        );
        assert_eq!(
            classify_pair(&a, &b, NEAR_LINES),
            Some((Severity::Info, Reason::BaseUnknown))
        );
        // 片側だけ不明でも取りこぼさない (段が混ざっても出る)
        let known = modified("src/app.rs", &[(10, 20)], 7);
        assert_eq!(
            classify_pair(&a, &known, NEAR_LINES),
            Some((Severity::Info, Reason::BaseUnknown))
        );
        assert_eq!(
            classify_pair(&known, &a, NEAR_LINES),
            Some((Severity::Info, Reason::BaseUnknown)),
            "順序を入れ替えても同じ"
        );

        // レポートまで畳んでも 1 件残り、逆引きにも段が付く
        let trees: Vec<TreeInfo> = (0..2)
            .map(|i| TreeInfo {
                id: i as u64,
                label: format!("w{i}"),
                files: vec!["src/app.rs".into()],
                ..TreeInfo::default()
            })
            .collect();
        let edits = vec![
            vec![base_unknown("src/app.rs")],
            vec![base_unknown("src/app.rs")],
        ];
        let rep = build_report(
            trees,
            &edits,
            &BTreeSet::new(),
            &BTreeMap::new(),
            NEAR_LINES,
        );
        assert_eq!(rep.hits.len(), 1, "1 件も出ないのが元のバグ: {rep:?}");
        assert_eq!(rep.hits[0].severity, Severity::Info);
        assert_eq!(rep.hits[0].reason, Reason::BaseUnknown);
        assert_eq!(rep.hotspots.len(), 1);
        assert_eq!(rep.hotspots[0].worst, Some(Severity::Info));
        // ただしバッジは鳴らさない (ファイル単位だけで警報にすると狼少年になる)
        assert!(rep.is_quiet());
        // 見出しは ✅ を出さない
        assert_eq!(headline(&rep), Headline::FileLevelOnly(1));
    }

    /// 見出しの分岐 (テーブル)。**ベース不明のときに「安全」と言わない**。
    #[test]
    fn 見出しはベース不明のときに衝突なしと言わない() {
        let mk = |trees: usize, base: Option<&str>, hits: &[(Severity, &str)]| Report {
            trees: (0..trees)
                .map(|i| TreeInfo {
                    id: i as u64,
                    ..TreeInfo::default()
                })
                .collect(),
            hits: hits
                .iter()
                .map(|(sev, path)| Hit {
                    path: (*path).into(),
                    a: 0,
                    b: 1,
                    severity: *sev,
                    reason: Reason::SameFile,
                    line: None,
                })
                .collect(),
            base: base.map(str::to_string),
            ..Report::default()
        };
        let cases: &[(usize, Option<&str>, &[(Severity, &str)], Headline)] = &[
            // 見張る対象が足りない
            (1, None, &[], Headline::TooFew),
            (0, None, &[], Headline::TooFew),
            // ベースが取れていて警報なし → 本当に静か
            (2, Some("abc1234"), &[], Headline::Quiet),
            (
                2,
                Some("abc1234"),
                &[(Severity::Info, "a.rs")],
                Headline::Quiet,
            ),
            // ベース不明 + ファイル単位の重なり → ✅ にしない (重複パスは畳む)
            (
                2,
                None,
                &[
                    (Severity::Info, "a.rs"),
                    (Severity::Info, "a.rs"),
                    (Severity::Info, "b.rs"),
                ],
                Headline::FileLevelOnly(2),
            ),
            // ベース不明でも何も触っていなければ静か
            (2, None, &[], Headline::Quiet),
            // 警報が立てば段によらず警報 (ベースの有無は関係ない)
            (2, None, &[(Severity::Warn, "a.rs")], Headline::Alarm(1)),
            (
                2,
                Some("abc1234"),
                &[(Severity::Certain, "a.rs"), (Severity::Info, "b.rs")],
                Headline::Alarm(1),
            ),
        ];
        for (trees, base, hits, want) in cases {
            let rep = mk(*trees, *base, hits);
            assert_eq!(
                headline(&rep),
                *want,
                "trees={trees} base={base:?} hits={hits:?}"
            );
        }
    }

    // -----------------------------------------------------------------
    // diff → 行範囲
    // -----------------------------------------------------------------

    #[test]
    fn unified0の差分からベース側の行範囲を起こす() {
        let d = "\
diff --git a/only_a.txt b/only_a.txt
index 7898192..0383519 100644
--- a/only_a.txt
+++ b/only_a.txt
@@ -1,0 +2,2 @@ a
+A
+uncommitted
diff --git a/shared.txt b/shared.txt
index 86bba90..9246ec4 100644
--- a/shared.txt
+++ b/shared.txt
@@ -2,2 +2,2 @@ l1
-l2
-l3
+l2-A
+l3-A
";
        let es = edits_from_diff(d);
        assert_eq!(es.len(), 2);
        let ins = es.iter().find(|e| e.path == "only_a.txt").expect("only_a");
        assert_eq!(ins.spans, vec![Span::insert_at(1)], "純粋な挿入は幅ゼロ");
        let m = es.iter().find(|e| e.path == "shared.txt").expect("shared");
        assert_eq!(m.spans, vec![Span::edit(2, 3)]);
        assert_eq!(m.kind, EditKind::Modified);
    }

    #[test]
    fn 新規と削除とリネームを種別として読む() {
        let d = "\
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,1 @@
+hi
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-a
-b
diff --git a/old.rs b/new.rs
similarity index 90%
rename from old.rs
rename to new.rs
";
        let es = edits_from_diff(d);
        let kinds: Vec<(&str, &EditKind)> = es.iter().map(|e| (e.path.as_str(), &e.kind)).collect();
        assert!(
            kinds.contains(&("new.txt", &EditKind::Created)),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("gone.txt", &EditKind::Deleted)),
            "{kinds:?}"
        );
        assert!(
            es.iter()
                .any(|e| matches!(&e.kind, EditKind::Renamed { from } if from == "old.rs")),
            "{kinds:?}"
        );
    }

    #[test]
    fn 改行がcrlfでも差分を読める() {
        // Windows のチェックアウトは CRLF。正規化せずに読めること。
        let d = "diff --git a/x.txt b/x.txt\r\n--- a/x.txt\r\n+++ b/x.txt\r\n@@ -3,1 +3,1 @@\r\n-old\r\n+new\r\n";
        let es = edits_from_diff(d);
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].spans, vec![Span::edit(3, 3)]);
    }

    // -----------------------------------------------------------------
    // merge-tree
    // -----------------------------------------------------------------

    #[test]
    fn mergetreeの出力を読み分ける() {
        let oid = "10b09fc15c518443eba83024915476a02613e0af";
        // 衝突あり (実際の出力から起こしたもの)
        let out = format!(
            "{oid}\0shared.txt\0\01\0shared.txt\0Auto-merging\0Auto-merging shared.txt\n\0"
        );
        assert_eq!(parse_merge_tree(&out), Some(vec!["shared.txt".to_string()]));
        // 綺麗にマージできる (OID だけ)
        assert_eq!(parse_merge_tree(oid), Some(Vec::new()));
        // エラー (**終了コードは衝突と同じ 1 なので出力で見分ける**)
        assert_eq!(
            parse_merge_tree("merge-tree: nosuchbranch - not something we can merge"),
            None
        );
        assert_eq!(parse_merge_tree(""), None);
    }

    #[test]
    fn mergetreeのバージョン判定は分からなければ使わない() {
        assert!(supports_merge_tree("git version 2.47.1"));
        assert!(supports_merge_tree("git version 2.38.0"));
        assert!(!supports_merge_tree("git version 2.37.9"));
        assert!(!supports_merge_tree("git version 2.30.1 (Apple Git-130)"));
        assert!(!supports_merge_tree(""), "読めなければ使わない");
        assert_eq!(
            merge_tree_argv("aaa", "bbb"),
            vec![
                "merge-tree",
                "--write-tree",
                "--name-only",
                "-z",
                "aaa",
                "bbb"
            ]
        );
    }

    // -----------------------------------------------------------------
    // ディスパッチ前チェック
    // -----------------------------------------------------------------

    #[test]
    fn ディスパッチ前チェックは実在する所有パスだけを拾う() {
        let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
        owners.insert("src/app.rs".into(), vec!["agent-b".into()]);
        owners.insert("src/git.rs".into(), vec!["agent-c".into()]);
        // 完全一致 / 相対 / 絶対・バッククォート囲み
        for text in [
            "src/app.rs を直して",
            "./src/app.rs のバグ",
            "`src/app.rs` を見て",
            "/home/x/proj/src/app.rs",
            "Read(src/app.rs)",
        ] {
            let w = dispatch_check(text, &owners);
            assert_eq!(w.len(), 1, "{text}");
            assert_eq!(w[0].path, "src/app.rs");
            assert_eq!(w[0].owners, vec!["agent-b".to_string()]);
        }
        // 裸のファイル名は拾わない (過少報告に倒す)
        assert!(dispatch_check("app.rs を直して", &owners).is_empty());
        // 所有されていないパスは拾わない
        assert!(dispatch_check("src/other.rs を直して", &owners).is_empty());
        // 似ているだけの語も拾わない
        assert!(dispatch_check("app.rsx", &owners).is_empty());
        assert!(dispatch_check("xsrc/app.rs", &owners).is_empty());
        // 空・所有者なし
        assert!(dispatch_check("", &owners).is_empty());
        assert!(dispatch_check("src/app.rs", &BTreeMap::new()).is_empty());
        // 2 件当たれば 2 件返る
        assert_eq!(dispatch_check("src/app.rs と src/git.rs", &owners).len(), 2);
    }

    #[test]
    fn ディスパッチ警告の要約は件数を畳む() {
        assert!(dispatch_summary(&[]).is_empty());
        let w: Vec<DispatchWarning> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|p| DispatchWarning {
                path: (*p).into(),
                owners: vec!["x".into()],
            })
            .collect();
        let s = dispatch_summary(&w);
        assert!(s.contains("a, b, c"), "{s}");
        assert!(s.contains('2'), "残り 2 件を畳む: {s}");
    }

    // -----------------------------------------------------------------
    // レイアウト
    // -----------------------------------------------------------------

    #[test]
    fn 行列は可用領域からはみ出さず重ならない() {
        // (可用幅, 可用高, 本数)
        let cases = [
            (900.0f32, 700.0f32, 2usize),
            (900.0, 700.0, 6),
            (1200.0, 300.0, 4),
            (1200.0, 300.0, 12),
            (420.0, 220.0, 3),
            (240.0, 200.0, 5),
            (3000.0, 1600.0, 20),
        ];
        for (w, h, n) in cases {
            let lay = matrix_layout(w, h, n);
            let origin = egui::pos2(0.0, 0.0);
            let rects: Vec<egui::Rect> = matrix_slots(&lay, origin)
                .into_iter()
                .map(|(_, r)| r)
                .collect();
            let area = egui::Rect::from_min_size(origin, egui::vec2(w, h));
            for (i, r) in rects.iter().enumerate() {
                assert!(
                    area.contains_rect(*r),
                    "({w}x{h}, n={n}) 矩形 {i} が領域外: {r:?} / {area:?} / {lay:?}"
                );
                for (j, o) in rects.iter().enumerate().skip(i + 1) {
                    let inter = r.intersect(*o);
                    assert!(
                        inter.width() <= 0.01 || inter.height() <= 0.01,
                        "({w}x{h}, n={n}) 矩形 {i} と {j} が重なる: {r:?} {o:?}"
                    );
                }
            }
            if lay.list_only {
                assert!(rects.is_empty());
            } else {
                assert!(lay.shown >= 2);
                assert_eq!(lay.shown + lay.folded, n);
            }
        }
    }

    #[test]
    fn 幅が足りなければ行列を諦める() {
        assert!(matrix_layout(100.0, 700.0, 4).list_only, "幅が無い");
        assert!(matrix_layout(900.0, 30.0, 4).list_only, "高さが無い");
        assert!(
            matrix_layout(900.0, 700.0, 1).list_only,
            "1 本なら行列は無い"
        );
        assert!(!matrix_layout(900.0, 700.0, 2).list_only);
    }

    // -----------------------------------------------------------------
    // その他
    // -----------------------------------------------------------------

    #[test]
    fn バイト数の表記は打ち切りを隠さない() {
        assert_eq!(human_bytes(512, false), "512 B");
        assert_eq!(human_bytes(2048, false), "2.0 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024, false), "3.0 GB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024, true), "3.0 GB+");
    }

    #[test]
    fn パスの正規化はセパレータを揃える() {
        assert_eq!(norm_path("src\\app.rs"), norm_path("src/app.rs"));
        assert_eq!(norm_path("./src/app.rs"), norm_path("src/app.rs"));
        if crate::worktree::fs_case_insensitive() {
            assert_eq!(norm_path("SRC/App.rs"), norm_path("src/app.rs"));
        }
    }

    #[test]
    fn カードのツールチップは警報が無ければ出ない() {
        let trees: Vec<TreeInfo> = (0..2)
            .map(|i| TreeInfo {
                id: 100 + i as u64,
                label: format!("w{i}"),
                ..TreeInfo::default()
            })
            .collect();
        let edits = vec![
            vec![modified("src/app.rs", &[(1, 2)], 1)],
            vec![modified("src/app.rs", &[(900, 901)], 2)],
        ];
        let rep = build_report(
            trees,
            &edits,
            &BTreeSet::new(),
            &BTreeMap::new(),
            NEAR_LINES,
        );
        assert_eq!(rep.card_hint(100), None, "情報どまりならバッジは出さない");
        assert_eq!(rep.card_hint(999), None, "知らない ID");
    }

    #[test]
    fn 見張る対象が二本未満ならgitを一度も起こさない() {
        let dir = crate::test_util::unique_temp_dir("zv-conflict", "idle");
        let mut radar = ConflictRadar::new();
        // 0 本 / 1 本 / 同じフォルダ 2 本 — どれも走査を起こさない。
        assert!(!radar.update(&[], false));
        assert!(!radar.scanning());
        let one = TreeSpec {
            id: 1,
            label: "a".into(),
            branch: "a".into(),
            dir: dir.clone(),
        };
        assert!(!radar.update(std::slice::from_ref(&one), false));
        assert!(!radar.scanning());
        let dup = TreeSpec {
            id: 2,
            label: "b".into(),
            ..one.clone()
        };
        assert!(!radar.update(&[one, dup], false));
        assert!(!radar.scanning(), "同じフォルダは 1 本に畳む");
        assert!(radar.report().hits.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------
    // 本物の git リポジトリでの統合テスト (ユーザーのリポジトリには触らない)
    // -----------------------------------------------------------------

    /// この環境に git があるか。**無ければテストごとスキップする** —
    /// `rust:1.90-slim` (`tools/linux-test.sh` の既定イメージ) には git が
    /// 入っておらず、`expect` で落とすと「Docker でだけ赤い」テストになる。
    /// CI の ubuntu には git があるので、そこでは必ず実行される。
    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e.x")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e.x")
            .output()
            .expect("git を起動できない");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn 本物のワークツリー二本で衝突を分類する() {
        if !git_available() {
            println!("git が無い環境なのでスキップ");
            return;
        }
        let root = crate::test_util::unique_temp_dir("zv-conflict", "real");
        let base = root.join("base");
        std::fs::create_dir_all(&base).expect("base");
        git(&base, &["init", "-q", "-b", "main"]);
        let body: String = (1..=30).map(|i| format!("l{i}\n")).collect();
        std::fs::write(base.join("shared.txt"), &body).expect("write");
        std::fs::write(base.join("gone.txt"), "bye\n").expect("write");
        git(&base, &["add", "-A"]);
        git(&base, &["commit", "-qm", "base"]);

        let wa = root.join("wa");
        let wb = root.join("wb");
        git(
            &base,
            &["worktree", "add", "-q", "-b", "wa", &wa.to_string_lossy()],
        );
        git(
            &base,
            &["worktree", "add", "-q", "-b", "wb", &wb.to_string_lossy()],
        );
        // A: 2〜3 行目を書き換え + gone.txt を削除 (どちらも未コミット)
        let a_body = body.replace("l2\n", "l2-A\n").replace("l3\n", "l3-A\n");
        std::fs::write(wa.join("shared.txt"), &a_body).expect("write");
        std::fs::remove_file(wa.join("gone.txt")).expect("rm");
        // B: 3 行目 (重なる) と 25 行目 (離れている) を書き換え + gone.txt を編集
        let b_body = body.replace("l3\n", "l3-B\n").replace("l25\n", "l25-B\n");
        std::fs::write(wb.join("shared.txt"), &b_body).expect("write");
        std::fs::write(wb.join("gone.txt"), "still here\n").expect("write");

        let specs = vec![
            TreeSpec {
                id: 1,
                label: "agent-A".into(),
                branch: "wa".into(),
                dir: wa.clone(),
            },
            TreeSpec {
                id: 2,
                label: "agent-B".into(),
                branch: "wb".into(),
                dir: wb.clone(),
            },
        ];
        let rep = scan(&specs, true);
        // 実際に何が出るかを目で見られるようにする (`-- --nocapture`)。
        println!("--- 衝突レーダー ({:?}) ---", rep.took);
        println!(
            "ベース: {:?}  {}",
            rep.base,
            rep.note.clone().unwrap_or_default()
        );
        for t in &rep.trees {
            println!(
                "  ツリー {} [{}] {} ファイル / dirty={} / {}",
                t.label,
                t.branch,
                t.files.len(),
                t.dirty,
                t.disk.map(|(b, c)| human_bytes(b, c)).unwrap_or_default()
            );
        }
        for h in &rep.hits {
            println!(
                "  {} {:8} {:12} {} ⇄ {}  行{:?}  — {}",
                h.severity.glyph(),
                h.severity.label(),
                h.path,
                rep.trees[h.a].label,
                rep.trees[h.b].label,
                h.line,
                h.reason.label()
            );
        }
        for hs in &rep.hotspots {
            println!("  🔥 {} ← {:?}", hs.path, hs.trees);
        }
        println!("  警報ファイル数: {}", rep.alarm_files());
        assert!(rep.base.is_some(), "共通ベースが取れる: {rep:?}");
        let shared = rep
            .hits
            .iter()
            .find(|h| h.path == "shared.txt")
            .unwrap_or_else(|| panic!("shared.txt の判定が無い: {:?}", rep.hits));
        assert_eq!(
            shared.severity,
            Severity::Certain,
            "3 行目を取り合っている: {shared:?}"
        );
        let gone = rep
            .hits
            .iter()
            .find(|h| h.path == "gone.txt")
            .unwrap_or_else(|| panic!("gone.txt の判定が無い: {:?}", rep.hits));
        assert_eq!(gone.severity, Severity::Certain);
        assert_eq!(gone.reason, Reason::DeleteEdit);
        assert_eq!(rep.alarm_files(), 2);
        // 逆引きも両方に出る
        assert_eq!(rep.hotspots.len(), 2);
        // 両側とも未コミットなので merge-tree は権威にできない旨が出る
        assert!(rep.note.is_some(), "降格の理由を必ず書く");
        assert!(rep.trees.iter().all(|t| t.dirty));

        git(
            &base,
            &["worktree", "remove", "--force", &wa.to_string_lossy()],
        );
        git(
            &base,
            &["worktree", "remove", "--force", &wb.to_string_lossy()],
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 離れた変更はコミット済みならgitの判定で情報へ落ちる() {
        if !git_available() {
            println!("git が無い環境なのでスキップ");
            return;
        }
        let root = crate::test_util::unique_temp_dir("zv-conflict", "clean");
        let base = root.join("base");
        std::fs::create_dir_all(&base).expect("base");
        git(&base, &["init", "-q", "-b", "main"]);
        let body: String = (1..=60).map(|i| format!("l{i}\n")).collect();
        std::fs::write(base.join("shared.txt"), &body).expect("write");
        git(&base, &["add", "-A"]);
        git(&base, &["commit", "-qm", "base"]);
        let wa = root.join("wa");
        let wb = root.join("wb");
        git(
            &base,
            &["worktree", "add", "-q", "-b", "wa", &wa.to_string_lossy()],
        );
        git(
            &base,
            &["worktree", "add", "-q", "-b", "wb", &wb.to_string_lossy()],
        );
        std::fs::write(wa.join("shared.txt"), body.replace("l2\n", "l2-A\n")).expect("w");
        git(&wa, &["commit", "-qam", "a"]);
        std::fs::write(wb.join("shared.txt"), body.replace("l50\n", "l50-B\n")).expect("w");
        git(&wb, &["commit", "-qam", "b"]);

        let specs = vec![
            TreeSpec {
                id: 1,
                label: "A".into(),
                branch: "wa".into(),
                dir: wa.clone(),
            },
            TreeSpec {
                id: 2,
                label: "B".into(),
                branch: "wb".into(),
                dir: wb.clone(),
            },
        ];
        let rep = scan(&specs, false);
        println!("--- 離れた変更・両側コミット済み ({:?}) ---", rep.took);
        println!("ベース: {:?}  note={:?}", rep.base, rep.note);
        for h in &rep.hits {
            println!(
                "  {} {:8} {:12} — {}",
                h.severity.glyph(),
                h.severity.label(),
                h.path,
                h.reason.label()
            );
        }
        println!("  警報ファイル数: {}", rep.alarm_files());
        assert!(rep.trees.iter().all(|t| !t.dirty), "両方コミット済み");
        let h = rep
            .hits
            .iter()
            .find(|h| h.path == "shared.txt")
            .unwrap_or_else(|| panic!("判定が無い: {rep:?}"));
        if supports_merge_tree(&crate::worktree::git_out(&base, &["--version"]).unwrap_or_default())
        {
            assert_eq!(h.severity, Severity::Info, "git が綺麗と言った: {h:?}");
            assert_eq!(h.reason, Reason::MergeClean);
        } else {
            // 古い git では 2 段目までなので「離れている = 情報」で同じ結論
            assert_eq!(h.severity, Severity::Info);
        }
        assert!(rep.is_quiet(), "警報は 1 件も出さない");

        git(
            &base,
            &["worktree", "remove", "--force", &wa.to_string_lossy()],
        );
        git(
            &base,
            &["worktree", "remove", "--force", &wb.to_string_lossy()],
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **回帰 (実 git)**: 共通ベースが 1 つも無い構成でも、ファイル単位の
    /// 重なりは必ず出る。
    ///
    /// [`scan`] の `merge-base --octopus` が失敗する経路 —
    /// 別々に `git init` した 2 本を並べると再現できる。以前はここで
    /// `hits` が空になり、パネルに 1 行も出ないまま
    /// 「ファイル単位までしか判定できません」とだけ表示していた。
    #[test]
    fn 共通ベースが無い二本でもファイル単位の重なりが出る() {
        if !git_available() {
            println!("git が無い環境なのでスキップ");
            return;
        }
        let root = crate::test_util::unique_temp_dir("zv-conflict", "nobase");
        let mut dirs = Vec::new();
        for name in ["ra", "rb"] {
            // **別々のリポジトリ** — 共通の祖先コミットが 1 つも無い
            let d = root.join(name);
            std::fs::create_dir_all(&d).expect("mkdir");
            git(&d, &["init", "-q", "-b", "main"]);
            std::fs::write(d.join("shared.txt"), "l1\nl2\nl3\n").expect("write");
            // **リポジトリごとに違うファイルを 1 つ置く。** 同じ内容・同じ
            // 作者・同じメッセージで commit すると OID まで一致してしまい、
            // `merge-base` が「共通ベースがある」と答えてしまう (実際に踏んだ)。
            std::fs::write(d.join(format!("{name}.txt")), name).expect("write");
            git(&d, &["add", "-A"]);
            git(&d, &["commit", "-qm", "base"]);
            // 未コミットの変更を置く (status からファイル名だけを起こす経路)
            std::fs::write(d.join("shared.txt"), format!("l1\nl2-{name}\nl3\n")).expect("write");
            dirs.push(d);
        }
        let specs: Vec<TreeSpec> = dirs
            .iter()
            .enumerate()
            .map(|(i, d)| TreeSpec {
                id: i as u64 + 1,
                label: format!("r{i}"),
                branch: "main".into(),
                dir: d.clone(),
            })
            .collect();
        let rep = scan(&specs, false);
        println!("--- 共通ベース無し ({:?}) ---", rep.took);
        println!("ベース: {:?}  note={:?}", rep.base, rep.note);
        for h in &rep.hits {
            println!(
                "  {} {:8} {:12} — {}",
                h.severity.glyph(),
                h.severity.label(),
                h.path,
                h.reason.label()
            );
        }
        assert!(
            rep.base.is_none(),
            "共通ベースが取れない構成のはず: {:?}",
            rep.base
        );
        assert!(rep.note.is_some(), "降格の理由を必ず書く");
        let hit = rep
            .hits
            .iter()
            .find(|h| h.path == "shared.txt")
            .unwrap_or_else(|| panic!("ファイル単位の重なりが 1 件も出ていない: {:?}", rep.hits));
        assert_eq!(hit.severity, Severity::Info);
        assert_eq!(hit.reason, Reason::BaseUnknown);
        assert_eq!(rep.hotspots.len(), 1, "逆引きにも出る");
        assert_eq!(headline(&rep), Headline::FileLevelOnly(1));
        let _ = std::fs::remove_dir_all(&root);
    }
}
