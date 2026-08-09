//! ローカルヒストリ — VCS に依らない「プロジェクト全体の取り消し履歴」。
//!
//! 「20 分前に壊した。コミットはしていない」を取り戻すための層。git は
//! **コミットした物**しか守らないので、コミットとコミットの間に落ちた変更は
//! 現状どこにも残らない。ここはその隙間を埋める。
//!
//! # IntelliJ の lvcs から写した性質 (と、写さなかった性質)
//!
//! | 性質 | 由来 | このモジュール |
//! |------|------|----------------|
//! | リビジョンの単位は**コマンド**。保存でも打鍵でもない | `ChangeListImpl` | [`Recorder`] の入れ子畳み込み |
//! | 入れ子の begin/end は畳み、**空の変更集合は捨てる** | 同上 | [`Recorder::end`] |
//! | 記録するのは**変更前**の内容。修正スタンプが同じなら何もしない | `IdeaGateway` | [`stamp_of`] / [`Shadow`] |
//! | 内容はアドレス指定で 1 個だけ持ち、参照数で数える | `ContentStorage` | [`Store`] |
//! | 保持は**活動時間**。12 時間を超える空白は 1ms と数える | `ChangeList#purgeObsolete` | [`purge_from`] |
//! | 削除はサブツリーごと写しを持つ (だから「消したフォルダを戻す」が効く) | `DeleteChange` | [`Change::tree`] |
//! | ラベルは同じ直線ログへ差す 1 エントリ | `PutLabelChange` | [`ChangeSet::label`] |
//! | 版が違うと**履歴を丸ごと捨てる** | `LocalHistoryImpl` | **写さない。** [`migrate_line`] で移行する |
//!
//! 最後の 1 行がこの実装の主張。IntelliJ は保存形式の版かファイルシステムの
//! 作成時刻が食い違うと履歴を全部消すので、「IDE を更新したらローカル
//! ヒストリが消えた」が定番の不満になっている。**更新で取り消し履歴が消える**
//! のは信頼を壊す振る舞いなので、こちらは版を記録して読み替える。
//! 未来の版を読んでしまったときも**消さずに読み取り専用**で開く
//! (古いビルドで一度起動しただけで新しいビルドの履歴が消える、を作らない)。
//!
//! # 取り込みの契機 — タイマーではなくイベント
//!
//! IntelliJ は VFS のイベントに常時ぶら下がるが、こちらは
//! **保存・エージェントのターン境界・履歴を開いた時**だけ走査する。
//! 設計原則 3 (アイドル時のコストはゼロ) を守るためで、何も起きていない間は
//! 裏のスレッドが `recv()` で完全に眠る (`recv_timeout` のポーリングすらしない)。
//!
//! # なぜ走査 (スキャン) 方式なのか
//!
//! エディタの編集だけを記録すると、**エージェントの shell が書いた変更**が
//! 丸ごと漏れる。Claude Code のチェックポイントが「bash で作った変更は戻せない」
//! と明言しているのはツール呼び出しに計装しているからで、同じ設計にすると
//! 同じ穴が開く。ここはファイルシステムを見る側に立つので、誰が書いたかに
//! 依らず拾える (`rm -rf` も削除として記録される)。
//!
//! # [`crate::checkpoint`] との継ぎ目
//!
//! [`Store`] + [`Shadow`] は「作業ツリー全体の、内容アドレス指定のスナップ
//! ショット」そのものなので、チェックポイントはこの上に載せられる。
//! 現時点で繋いであるのは**ターン境界のスナップショット**
//! ([`LocalHistory::snapshot`] を `submit_tick` から呼ぶ) までで、
//! `checkpoint.rs` の git 実装の置き換えは行っていない
//! (git 側は「ステージ済みを 1 ビットも変えない」という別の保証を持つため、
//!  片方へ寄せるのは別の判断が要る)。
//!
//! # スレッド
//!
//! ファイル走査も書き出しも**必ず裏のスレッド**。UI は `mpsc` で結果を受け、
//! 手元にある値を描く。UI スレッドから `std::fs` を叩く経路はこのモジュールに
//! 1 本も無い (`git.rs` の `Git::branch` と同じ理由 — 実測でフレームが
//! 4376ms 止まった前例がある)。

use crate::i18n::{tr, trf};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

// ══════════════════════════════════════════════════════════════════
//  定数
// ══════════════════════════════════════════════════════════════════

/// 記録形式の版。**上げたら [`migrate_line`] に読み替えを足すこと。**
pub const FORMAT_VERSION: u32 = 2;

/// 内容を保存する上限。これを超えるファイルは「構造だけ」記録する
/// (IntelliJ がバイナリに対して行うのと同じ扱い)。
const MAX_BLOB_BYTES: u64 = 4 * 1024 * 1024;

/// 1 回の走査で見るファイル数の上限。ここに当たったら**削除は記録しない**
/// (見きれなかったファイルを「消えた」と誤認するのが最悪の事故なので、
///  安全側に倒して何もしない)。
const MAX_FILES: usize = 50_000;

/// 変更をため込んでから書き出すまでの猶予。IntelliJ も同程度の間隔で
/// 裏から流している。UI スレッドがロックの下で書かないための仕組みでもある。
const FLUSH_DELAY: Duration = Duration::from_millis(900);

/// 予約したコマンド名が有効な時間 (ms)。これを過ぎた走査は「外部変更」に
/// なる — 30 分前の「保存」を今の変更の名前にしない。
const NAME_GRACE_MS: i64 = 15_000;

/// 内容 ID が衝突したときに試す別名の数。ここまで試して駄目なら諦めて
/// 「構造だけ」に落とす (握り潰さずログへ出す)。
const ID_PROBE: usize = 64;

/// 差分に流し込む最大バイト数。これを超えたら切って断りを出す。
const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;

/// 一覧の 1 行に出すパスの最大数 (それ以上は「ほか N 件」)。
const MAX_SHOWN_PATHS: usize = 200;

// ══════════════════════════════════════════════════════════════════
//  保持ポリシー (設定から作る。リテラルを埋め込まない)
// ══════════════════════════════════════════════════════════════════

/// 保持ポリシー。**日数も空白のしきい値も設定値**で、ここに数字は書かない
/// (既定値は [`crate::config::Config`] 側が持つ)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    /// 保持する「活動時間」(ms)。壁時計ではない。
    pub period_ms: i64,
    /// これを超える空白は 1ms と数える (ms)。
    pub gap_ms: i64,
}

impl Retention {
    /// 設定から作る。0 以下が入っていても壊れないよう下限で止める。
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        let days = cfg.local_history_days.max(1) as i64;
        let hours = cfg.local_history_gap_hours.max(1) as i64;
        Self {
            period_ms: days * 24 * 60 * 60 * 1000,
            gap_ms: hours * 60 * 60 * 1000,
        }
    }
}

// ══════════════════════════════════════════════════════════════════
//  記録の型 (JSONL の 1 行 = 1 変更集合)
// ══════════════════════════════════════════════════════════════════

/// 変更の種類。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// 内容が変わった。[`Change::before`] が**変更前**の内容 ID。
    #[default]
    Content,
    /// 新しく現れた。変更前の内容は無い。
    Create,
    /// 消えた。[`Change::tree`] にサブツリーの写しが入る。
    Delete,
}

/// 削除されたサブツリーの 1 節点。フォルダごと戻すために必要な物だけ持つ。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    /// この節点の名前 (パス要素 1 個)。
    pub name: String,
    /// ディレクトリか。
    pub dir: bool,
    /// 内容 ID。空なら内容を持たない (ディレクトリ / バイナリ / 上限超過)。
    pub content: String,
    /// 子。ディレクトリのときだけ入る。
    pub children: Vec<Entry>,
}

/// 1 ファイル (または 1 サブツリー) に起きたこと。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Change {
    /// ワークスペース相対パス (**必ず `/` 区切り**。Windows でも `\` を持たない)。
    pub path: String,
    /// 種類。
    pub kind: ChangeKind,
    /// 変更前の内容 ID。空なら「前の内容は記録していない」。
    pub before: String,
    /// 内容を記録しなかった (バイナリ / 上限超過)。構造だけの記録。
    pub structure_only: bool,
    /// 削除のときのサブツリーの写し。
    pub tree: Option<Entry>,
}

/// 1 コマンドぶんの変更集合。一覧の 1 行になる。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChangeSet {
    /// 時刻 (**Unix ミリ秒**)。v1 は秒だったので [`migrate_line`] が 1000 倍する。
    pub at: i64,
    /// コマンド名。**これが一覧を「活動ログ」にしている一番大事な値**。
    pub name: String,
    /// ユーザー / システムが差したラベルか。
    pub label: bool,
    /// 中身。ラベルのときは空。
    pub changes: Vec<Change>,
}

impl ChangeSet {
    /// `path` に効いている変更 (完全一致、または祖先の削除)。
    fn change_for(&self, path: &str) -> Option<&Change> {
        self.changes
            .iter()
            .find(|c| c.path == path || is_ancestor(&c.path, path))
    }
}

/// `a` が `b` の (真の) 祖先パスか。`""` は誰の祖先でもない。
fn is_ancestor(a: &str, b: &str) -> bool {
    !a.is_empty() && b.len() > a.len() && b.starts_with(a) && b.as_bytes()[a.len()] == b'/'
}

/// 保存形式の目印。**版が違っても捨てない**ためにこれだけは別ファイルに置く。
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Meta {
    version: u32,
}

// ══════════════════════════════════════════════════════════════════
//  純粋関数 — 変更集合の入れ子
// ══════════════════════════════════════════════════════════════════

/// 変更集合を組み立てる。**入れ子は畳み、空の集合は捨てる。**
///
/// IntelliJ の `ChangeListImpl` は `changeSetDepth` を数え、いちばん外側の
/// `endChangeSet` だけが書き出す。名前も外側が勝つ — 「名前の変更」の中で
/// 走った「整形」が別のリビジョンとして現れると、一覧が実行の粒度ではなく
/// 実装の粒度で埋まってしまうため。
#[derive(Debug, Default)]
pub struct Recorder {
    depth: usize,
    cur: Option<ChangeSet>,
    out: Vec<ChangeSet>,
}

impl Recorder {
    /// 空の記録器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 変更集合を開く。既に開いていれば**深さを数えるだけ** (名前は外側が勝つ)。
    pub fn begin(&mut self, name: &str, at: i64) {
        self.depth += 1;
        if self.depth == 1 {
            self.cur = Some(ChangeSet {
                at,
                name: name.to_string(),
                label: false,
                changes: Vec::new(),
            });
        }
    }

    /// 変更を 1 件積む。開いていなければ**捨てる** (`false` を返す)。
    pub fn add(&mut self, c: Change) -> bool {
        match self.cur.as_mut() {
            Some(s) => {
                s.changes.push(c);
                true
            }
            None => false,
        }
    }

    /// 変更集合を閉じる。いちばん外側でだけ書き出し、**空なら捨てる**。
    pub fn end(&mut self) {
        if self.depth == 0 {
            return;
        }
        self.depth -= 1;
        if self.depth > 0 {
            return;
        }
        if let Some(s) = self.cur.take() {
            if !s.changes.is_empty() {
                self.out.push(s);
            }
        }
    }

    /// ラベルを 1 件差す。**中身が空でも捨てない** — 印そのものが中身だから。
    pub fn label(&mut self, name: &str, at: i64) {
        self.out.push(ChangeSet {
            at,
            name: name.to_string(),
            label: true,
            changes: Vec::new(),
        });
    }

    /// 出来上がった変更集合を取り出す (古い順)。
    pub fn take(&mut self) -> Vec<ChangeSet> {
        std::mem::take(&mut self.out)
    }
}

// ══════════════════════════════════════════════════════════════════
//  純粋関数 — 修正スタンプ・内容 ID・保持
// ══════════════════════════════════════════════════════════════════

/// 修正スタンプ。IntelliJ が「スタンプが同じなら null を返して何も記録しない」
/// のと同じ役割で、こちらは (サイズ, 更新時刻) から作る。
///
/// 上位 32bit にサイズを回すのは、サイズだけ / 時刻だけが変わった場合を
/// 確実に別のスタンプにするため。
pub fn stamp_of(size: u64, mtime_ms: i64) -> u64 {
    size.rotate_left(32) ^ (mtime_ms as u64)
}

/// 内容 ID の**基底**。長さを混ぜるので、ハッシュ衝突は長さも同じ場合に限られる。
///
/// 暗号ハッシュを使わないのは依存を増やさないため。その代わり
/// [`Store::put`] は**当たった blob の中身をバイト単位で照合**するので、
/// ハッシュが衝突しても別内容を同じ物と見なすことは起こらない
/// (ハッシュの実装が将来変わっても、再保存されるだけで壊れない)。
pub fn content_id(bytes: &[u8]) -> String {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}{:x}", h.finish(), bytes.len())
}

/// **活動時間**で古い変更集合を切る境界 (純関数)。
///
/// `times` は古い順の時刻 (ms)。返り値は「残す先頭の添字」で、それより前は
/// 捨てて良い。新しい方から隣同士の差を足していき、`period_ms` を超えた所で
/// 切る。**`gap_ms` を超える空白は 1ms と数える**ので、1 週間マシンを離れても
/// 予算を食わない (IntelliJ の `ChangeList#purgeObsolete` と同じ規則)。
///
/// いちばん新しい 1 件は必ず残る。
pub fn purge_from(times: &[i64], period_ms: i64, gap_ms: i64) -> usize {
    if times.len() < 2 {
        return 0;
    }
    let mut acc: i64 = 0;
    for i in (0..times.len() - 1).rev() {
        let delta = times[i + 1].saturating_sub(times[i]).max(0);
        acc = acc.saturating_add(if delta > gap_ms { 1 } else { delta });
        if acc > period_ms {
            return i + 1;
        }
    }
    0
}

// ══════════════════════════════════════════════════════════════════
//  純粋関数 — 削除サブツリーの切り出し
// ══════════════════════════════════════════════════════════════════

/// 消えたファイル群を「削除の根」でまとめる (純関数)。
///
/// フォルダごと消えたなら 1 件の削除として扱いたい。各パスの祖先を浅い方から
/// 辿り、**最初に見つかった「もう存在しないディレクトリ」**を根にする。
/// 祖先が全部残っているならファイル単体の削除。
///
/// `exists` はディレクトリの生存判定 (テストから差し替えるので閉包で受ける)。
pub fn delete_roots(
    missing: &[String],
    exists: &dyn Fn(&str) -> bool,
) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for m in missing {
        let parts: Vec<&str> = m.split('/').collect();
        let mut acc = String::new();
        let mut root: Option<String> = None;
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                acc.push('/');
            }
            acc.push_str(part);
            // 最後の要素はファイル自身なので祖先ではない
            if i + 1 < parts.len() && !exists(&acc) {
                root = Some(acc.clone());
                break;
            }
        }
        out.entry(root.unwrap_or_else(|| m.clone()))
            .or_default()
            .push(m.clone());
    }
    for v in out.values_mut() {
        v.sort();
    }
    out
}

/// 削除の根とその配下のファイルから、入れ子の写しを組む (純関数)。
///
/// `files` は `(相対パス, 内容 ID)`。根がファイル 1 個ならそのまま葉になる。
pub fn build_tree(root: &str, files: &[(String, String)]) -> Entry {
    let name = root.rsplit('/').next().unwrap_or(root).to_string();
    if files.len() == 1 && files[0].0 == root {
        return Entry {
            name,
            dir: false,
            content: files[0].1.clone(),
            children: Vec::new(),
        };
    }
    let mut top = Entry {
        name,
        dir: true,
        content: String::new(),
        children: Vec::new(),
    };
    for (rel, id) in files {
        let sub = rel
            .strip_prefix(root)
            .and_then(|s| s.strip_prefix('/'))
            .unwrap_or(rel.as_str());
        let parts: Vec<&str> = sub.split('/').filter(|s| !s.is_empty()).collect();
        let mut node = &mut top;
        for (i, part) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            let pos = node.children.iter().position(|c| c.name == *part);
            let idx = match pos {
                Some(p) => p,
                None => {
                    node.children.push(Entry {
                        name: (*part).to_string(),
                        dir: !last,
                        content: String::new(),
                        children: Vec::new(),
                    });
                    node.children.len() - 1
                }
            };
            node = &mut node.children[idx];
            if last {
                node.dir = false;
                node.content = id.clone();
            }
        }
    }
    top
}

/// 写しの中の内容 ID を全部集める (参照数の増減に使う)。
fn tree_ids(e: &Entry, out: &mut Vec<String>) {
    if !e.content.is_empty() {
        out.push(e.content.clone());
    }
    for c in &e.children {
        tree_ids(c, out);
    }
}

/// 写しの中から相対パス `rel` の節点を引く (`""` なら根そのもの)。
fn tree_at<'a>(e: &'a Entry, rel: &str) -> Option<&'a Entry> {
    if rel.is_empty() {
        return Some(e);
    }
    let (head, rest) = match rel.split_once('/') {
        Some((h, r)) => (h, r),
        None => (rel, ""),
    };
    let child = e.children.iter().find(|c| c.name == head)?;
    tree_at(child, rest)
}

/// 1 つの変更が参照している内容 ID (参照数の解放に使う)。
///
/// 削除の参照は**写しが唯一の持ち主**にしてある (`before` と二重に数えない)。
fn refs_of(c: &Change) -> Vec<String> {
    if c.kind == ChangeKind::Delete {
        let mut v = Vec::new();
        if let Some(t) = &c.tree {
            tree_ids(t, &mut v);
        }
        return v;
    }
    if c.before.is_empty() {
        Vec::new()
    } else {
        vec![c.before.clone()]
    }
}

// ══════════════════════════════════════════════════════════════════
//  純粋関数 — 再生 (履歴の読み出し)
// ══════════════════════════════════════════════════════════════════

/// 一覧に出す 1 行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    /// ログ内の位置 (古い順の添字)。復元の指定に使う。
    pub index: usize,
    /// 時刻 (Unix ミリ秒)。
    pub at: i64,
    /// コマンド名。
    pub name: String,
    /// ラベルか。
    pub label: bool,
    /// この範囲で触れたパス。
    pub paths: Vec<String>,
    /// 削除を含むか (「消したフォルダを戻す」の目印)。
    pub deleted: bool,
}

/// `prefix` に効く変更集合を**新しい順**に並べる (純関数)。
///
/// `prefix` が空ならプロジェクト全体。ファイル 1 本を指したときは、
/// **その祖先フォルダの削除も拾う** — 「消えたフォルダの中の 1 ファイル」を
/// 履歴から戻せるようにするため。ラベルは範囲に依らず常に出す
/// (直線ログの目印なので、絞り込みで消えると意味が無くなる)。
pub fn revisions(sets: &[ChangeSet], prefix: &str) -> Vec<Revision> {
    let mut out: Vec<Revision> = Vec::new();
    for (i, s) in sets.iter().enumerate() {
        if s.label {
            out.push(Revision {
                index: i,
                at: s.at,
                name: s.name.clone(),
                label: true,
                paths: Vec::new(),
                deleted: false,
            });
            continue;
        }
        let mut paths: Vec<String> = Vec::new();
        let mut deleted = false;
        for c in &s.changes {
            let hit = prefix.is_empty()
                || c.path == prefix
                || is_ancestor(prefix, &c.path)
                || is_ancestor(&c.path, prefix);
            if !hit {
                continue;
            }
            // 祖先の削除で絞り込んだときは、**見ている当人のパス**を出す。
            let shown = if is_ancestor(&c.path, prefix) {
                prefix.to_string()
            } else {
                c.path.clone()
            };
            if !paths.contains(&shown) {
                paths.push(shown);
            }
            deleted |= c.kind == ChangeKind::Delete;
        }
        if paths.is_empty() {
            continue;
        }
        paths.sort();
        out.push(Revision {
            index: i,
            at: s.at,
            name: s.name.clone(),
            label: false,
            paths,
            deleted,
        });
    }
    out.reverse();
    out
}

/// あるリビジョンの**直前**における 1 パスの状態。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathState {
    /// その内容だった。
    Content(String),
    /// まだ (もう) 無かった。
    Absent,
    /// それ以降 1 度も触っていない = **今と同じ**。
    Unchanged,
}

/// `sets`(古い順) の添字 `idx` の**直前**における `path` の状態 (純関数)。
///
/// 変更は「変更前の内容」を持つので、新しい方から `idx` まで遡り、
/// **最後に見た (= いちばん古い) 記録**がその時点の姿になる。
pub fn content_before(sets: &[ChangeSet], idx: usize, path: &str) -> PathState {
    let mut st = PathState::Unchanged;
    for s in sets.iter().skip(idx).rev() {
        let Some(c) = s.change_for(path) else {
            continue;
        };
        st = match c.kind {
            ChangeKind::Create => PathState::Absent,
            ChangeKind::Content => {
                if c.before.is_empty() {
                    PathState::Absent
                } else {
                    PathState::Content(c.before.clone())
                }
            }
            ChangeKind::Delete => {
                // 祖先ごと消えたときは写しの中から当人を引く。
                let rel = if c.path == path {
                    ""
                } else {
                    path[c.path.len() + 1..].as_ref()
                };
                match c.tree.as_ref().and_then(|t| tree_at(t, rel)) {
                    Some(e) if !e.content.is_empty() => PathState::Content(e.content.clone()),
                    Some(_) => PathState::Absent,
                    None if !c.before.is_empty() => PathState::Content(c.before.clone()),
                    None => PathState::Absent,
                }
            }
        };
    }
    st
}

// ══════════════════════════════════════════════════════════════════
//  純粋関数 — 版の移行 (**ここが IntelliJ に勝つ所**)
// ══════════════════════════════════════════════════════════════════

/// v1 の 1 行。時刻が**秒**で、変更に種別が無かった。
#[derive(Deserialize)]
#[serde(default)]
struct V1ChangeSet {
    at: i64,
    name: String,
    label: bool,
    changes: Vec<V1Change>,
}

impl Default for V1ChangeSet {
    fn default() -> Self {
        Self {
            at: 0,
            name: String::new(),
            label: false,
            changes: Vec::new(),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct V1Change {
    path: String,
    before: String,
}

/// 古い版の 1 行を今の形へ読み替える (純関数)。
///
/// **読めない行は捨てるが、読める行は必ず残す。** 版が違うだけで履歴を全部
/// 落とすのは「更新したら取り消し履歴が消えた」という一番やってはいけない
/// 壊し方なので、ここで吸収する。未知の (= 未来の) 版は `None` を返し、
/// 呼び出し側が**読み取り専用**で開く。
pub fn migrate_line(version: u32, line: &str) -> Option<ChangeSet> {
    match version {
        1 => {
            let v1: V1ChangeSet = serde_json::from_str(line).ok()?;
            Some(ChangeSet {
                // v1 は秒。同じ秒に複数コマンドが入ると順序が決まらなかったので
                // v2 でミリ秒にした。移行は 1000 倍でそのまま順序が保たれる。
                at: v1.at.saturating_mul(1000),
                name: v1.name,
                label: v1.label,
                changes: v1
                    .changes
                    .into_iter()
                    .map(|c| Change {
                        path: c.path,
                        // v1 は内容変更しか記録していない。
                        kind: ChangeKind::Content,
                        before: c.before,
                        structure_only: false,
                        tree: None,
                    })
                    .collect(),
            })
        }
        v if v == FORMAT_VERSION => serde_json::from_str(line).ok(),
        _ => None,
    }
}

// ══════════════════════════════════════════════════════════════════
//  純粋関数 — 画面の割り付け
// ══════════════════════════════════════════════════════════════════

/// 2 列にする最小幅。これを下回ったら一覧と詳細を縦に積む。
const TWO_COL_MIN_W: f32 = 620.0;
/// 一覧と詳細の間隔。
const PANE_GAP: f32 = 8.0;
/// 一覧の最小幅 / 最大幅 (2 列のとき)。
const LIST_MIN_W: f32 = 200.0;
const LIST_MAX_W: f32 = 380.0;
/// 1 列のときに一覧へ渡す高さの比率と下限。
const LIST_H_RATIO: f32 = 0.55;
const LIST_MIN_H: f32 = 90.0;

/// 一覧と詳細の矩形を決める (純関数)。
///
/// * `has_detail == false` なら**詳細の矩形を返さない** — 中身の無い枠で
///   高さを取らない (「空白は作らない」)。
/// * 返す矩形は必ず `area` の中に収まり、互いに重ならない。
pub fn history_rects(area: egui::Rect, has_detail: bool) -> (egui::Rect, Option<egui::Rect>) {
    if !has_detail {
        return (area, None);
    }
    if area.width() >= TWO_COL_MIN_W {
        let list_w = (area.width() * 0.34).clamp(LIST_MIN_W, LIST_MAX_W);
        let list = egui::Rect::from_min_size(area.min, egui::vec2(list_w, area.height()));
        let detail = egui::Rect::from_min_max(
            egui::pos2(list.max.x + PANE_GAP, area.min.y),
            egui::pos2(area.max.x, area.max.y),
        );
        return (list, Some(detail));
    }
    // 縦積み。詳細に最低限の高さが残らないなら 1 列で通す。
    let list_h = (area.height() * LIST_H_RATIO).max(LIST_MIN_H);
    if area.height() - list_h - PANE_GAP < LIST_MIN_H {
        return (area, None);
    }
    let list = egui::Rect::from_min_size(area.min, egui::vec2(area.width(), list_h));
    let detail = egui::Rect::from_min_max(
        egui::pos2(area.min.x, list.max.y + PANE_GAP),
        egui::pos2(area.max.x, area.max.y),
    );
    (list, Some(detail))
}

// ══════════════════════════════════════════════════════════════════
//  内容ストア (アドレス指定 + 参照数)
// ══════════════════════════════════════════════════════════════════

/// 内容を 1 個だけ持ち、参照数で寿命を決める倉庫。
///
/// **N 個のリビジョンが同じ内容を指しても整数 N 個ぶんしか増えない**。
/// これがこの機能を現実的な容量で成立させている (IntelliJ の
/// `ContentStorage` と同じ考え方)。
pub struct Store {
    dir: PathBuf,
    refs: HashMap<String, u32>,
    dirty: bool,
}

impl Store {
    fn new(dir: PathBuf) -> Self {
        let refs = std::fs::read_to_string(dir.join("refs.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, u32>>(&s).ok())
            .unwrap_or_default();
        Self {
            dir,
            refs,
            dirty: false,
        }
    }

    fn blob_path(&self, id: &str) -> PathBuf {
        let shard: String = id.chars().take(2).collect();
        self.dir.join("contents").join(shard).join(id)
    }

    /// 内容を入れて ID を返す (**参照数 +1 の状態で返る**)。
    ///
    /// 同じハッシュの blob が既にあっても**中身をバイト単位で照合**してから
    /// 使い回す。照合が外れたら別名 (`~1`, `~2` …) を試す。ここを省くと
    /// 「ハッシュが衝突した瞬間に別ファイルの中身が復元される」という
    /// 直し方の無い壊れ方をする。
    fn put(&mut self, bytes: &[u8]) -> Result<String, String> {
        let base = content_id(bytes);
        for n in 0..ID_PROBE {
            let id = if n == 0 {
                base.clone()
            } else {
                format!("{base}~{n}")
            };
            let p = self.blob_path(&id);
            match std::fs::read(&p) {
                Ok(cur) if cur == bytes => {
                    self.acquire(&id);
                    return Ok(id);
                }
                Ok(_) => continue,
                Err(_) => {
                    if let Some(d) = p.parent() {
                        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
                    }
                    std::fs::write(&p, bytes).map_err(|e| e.to_string())?;
                    self.acquire(&id);
                    return Ok(id);
                }
            }
        }
        Err(tr(
            "内容 ID が衝突し続けたため、この変更は構造だけ記録します",
        ))
    }

    fn acquire(&mut self, id: &str) {
        if id.is_empty() {
            return;
        }
        *self.refs.entry(id.to_string()).or_insert(0) += 1;
        self.dirty = true;
    }

    /// 参照を 1 つ手放す。0 になったら blob を消す。
    fn release(&mut self, id: &str) {
        if id.is_empty() {
            return;
        }
        let gone = match self.refs.get_mut(id) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            Some(_) => true,
            None => false,
        };
        if gone {
            self.refs.remove(id);
            std::fs::remove_file(self.blob_path(id)).ok();
        }
        self.dirty = true;
    }

    fn read(&self, id: &str) -> Option<Vec<u8>> {
        if id.is_empty() {
            return None;
        }
        std::fs::read(self.blob_path(id)).ok()
    }

    fn save(&mut self) {
        if !self.dirty {
            return;
        }
        if let Ok(s) = serde_json::to_string(&self.refs) {
            write_atomic(&self.dir.join("refs.json"), s.as_bytes()).ok();
        }
        self.dirty = false;
    }
}

// ══════════════════════════════════════════════════════════════════
//  影の索引 (いまの作業ツリーの姿)
// ══════════════════════════════════════════════════════════════════

/// 1 ファイルの「前回見たときの姿」。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shadow {
    /// 修正スタンプ ([`stamp_of`])。
    pub stamp: u64,
    /// 内容 ID。空なら内容を持っていない (バイナリ / 上限超過)。
    pub content: String,
}

impl Shadow {
    /// スタンプが変わったか。**変わっていなければ内容を読まない**
    /// (IntelliJ が「スタンプが同じなら何も記録しない」のと同じ間引き)。
    pub fn stamp_changed(&self, stamp: u64) -> bool {
        self.stamp != stamp
    }
}

// ══════════════════════════════════════════════════════════════════
//  裏のスレッドへ投げる仕事 / 返ってくる結果
// ══════════════════════════════════════════════════════════════════

enum Msg {
    /// コマンド名を予約して取り込みを予定する。`now` なら即座に走らせる。
    /// `want_log` は一覧を開いているときだけ真 — 閉じているのに全ログを
    /// 毎回積んで送ると、保存のたびに無駄な複製が UI へ流れる。
    Note {
        name: String,
        now: bool,
        want_log: bool,
    },
    /// ログを読み直して返す。
    Load,
    /// ラベルを 1 件差す。
    Label(String),
    /// `idx` の直前と今の差分を返す。
    Diff { idx: usize, path: String },
    /// 1 パスを `idx` の直前へ戻す (フォルダなら写しごと)。
    Restore { idx: usize, path: String },
    /// `idx` 以降に触れた物を全部 `idx` の直前へ戻す。
    Revert { idx: usize, prefix: String },
}

/// 裏のスレッドが返す結果。
#[derive(Debug, Clone)]
pub enum Done {
    /// 取り込んだ (`sets` 件の変更集合 / `changes` 件の変更)。
    Scanned { sets: usize, changes: usize },
    /// ログが揃った。
    Loaded(Vec<ChangeSet>),
    /// 差分が取れた。app 側が既存の差分ビューへ渡す。
    Diff {
        /// 見出し。
        title: String,
        /// 対象パス。
        path: String,
        /// その時点の本文。
        old: String,
        /// 今の本文。
        new: String,
    },
    /// 書き戻した。
    Restored {
        /// 書いた件数。
        written: usize,
        /// その時点で存在しなかったので**消さずに残した**件数。
        kept: usize,
    },
    /// 失敗。
    Failed(String),
}

// ══════════════════════════════════════════════════════════════════
//  エンジン (裏のスレッドの中身)
// ══════════════════════════════════════════════════════════════════

struct Engine {
    root: PathBuf,
    dir: PathBuf,
    store: Store,
    shadow: BTreeMap<String, Shadow>,
    log: Vec<ChangeSet>,
    ret: Retention,
    ignorer: crate::ignore::Ignorer,
    /// 未来の版を読んだ。**1 バイトも書かない** (古いビルドで起動しただけで
    /// 新しいビルドの履歴が消える、を作らない)。
    readonly: bool,
    /// 予約されたコマンド名と、予約した時刻。
    pending: Option<(String, i64)>,
}

impl Engine {
    fn new(root: PathBuf, dir: PathBuf, ret: Retention, gitignore: bool) -> Self {
        let mut e = Engine {
            root,
            dir,
            store: Store::new(PathBuf::new()),
            shadow: BTreeMap::new(),
            log: Vec::new(),
            ret,
            ignorer: crate::ignore::Ignorer::new(gitignore),
            readonly: false,
            pending: None,
        };
        e.store = Store::new(e.dir.clone());
        e.load_meta();
        e.load_log();
        e.load_shadow();
        e
    }

    // ── 永続化 ──────────────────────────────────────────────

    fn load_meta(&mut self) {
        let p = self.dir.join("meta.json");
        let found = std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str::<Meta>(&s).ok())
            .map(|m| m.version);
        match found {
            Some(v) if v > FORMAT_VERSION => self.readonly = true,
            _ => {}
        }
    }

    /// ログを読む。**古い版は読み替えて取り込む** (捨てない)。
    fn load_log(&mut self) {
        let version = std::fs::read_to_string(self.dir.join("meta.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Meta>(&s).ok())
            .map(|m| m.version)
            .unwrap_or(FORMAT_VERSION);
        let Ok(raw) = std::fs::read_to_string(self.dir.join("log.jsonl")) else {
            return;
        };
        // Windows のチェックアウト / 別ツールが書いた CRLF でも同じに読む。
        let raw = raw.replace("\r\n", "\n");
        let migrated = version != FORMAT_VERSION && !self.readonly;
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(s) = migrate_line(version, line) {
                self.log.push(s);
            }
        }
        self.log.sort_by_key(|s| s.at);
        if migrated {
            // 読み替えた形で書き直し、版を上げる。次からは変換が要らない。
            self.write_log();
            self.write_meta();
        }
    }

    fn load_shadow(&mut self) {
        if let Ok(s) = std::fs::read_to_string(self.dir.join("index.json")) {
            if let Ok(m) = serde_json::from_str::<BTreeMap<String, Shadow>>(&s) {
                self.shadow = m;
            }
        }
    }

    fn write_meta(&self) {
        if self.readonly {
            return;
        }
        if let Ok(s) = serde_json::to_string(&Meta {
            version: FORMAT_VERSION,
        }) {
            write_atomic(&self.dir.join("meta.json"), s.as_bytes()).ok();
        }
    }

    fn write_log(&self) {
        if self.readonly {
            return;
        }
        let mut out = String::new();
        for s in &self.log {
            if let Ok(line) = serde_json::to_string(s) {
                out.push_str(&line);
                out.push('\n');
            }
        }
        write_atomic(&self.dir.join("log.jsonl"), out.as_bytes()).ok();
    }

    fn write_shadow(&self) {
        if self.readonly {
            return;
        }
        if let Ok(s) = serde_json::to_string(&self.shadow) {
            write_atomic(&self.dir.join("index.json"), s.as_bytes()).ok();
        }
    }

    // ── 走査 ────────────────────────────────────────────────

    /// ワークスペースを 1 周見て、変更集合を組んで書き出す。
    fn scan(&mut self, name: &str) -> Done {
        if self.readonly {
            return Done::Failed(tr(
                "この履歴は新しい版で書かれています。読むだけにして書き換えません",
            ));
        }
        let at = now_ms();
        let (found, truncated) = self.walk();
        let mut rec = Recorder::new();
        rec.begin(name, at);

        let mut seen: HashSet<String> = HashSet::with_capacity(found.len());
        for (rel, abs, size, mtime) in &found {
            seen.insert(rel.clone());
            let stamp = stamp_of(*size, *mtime);
            match self.shadow.get(rel) {
                // スタンプが同じなら中身も読まない (重複抑止)
                Some(sh) if !sh.stamp_changed(stamp) => continue,
                Some(sh) => {
                    let prev = sh.content.clone();
                    let (new_id, structure_only) = self.store_file(abs, *size);
                    if new_id == prev && !structure_only {
                        // 中身は同じ。スタンプだけ更新して記録しない。
                        self.store.release(&new_id);
                        if let Some(e) = self.shadow.get_mut(rel) {
                            e.stamp = stamp;
                        }
                        continue;
                    }
                    // 変更前の内容は**記録が持ち主**になる。影の索引は手放す。
                    self.store.acquire(&prev);
                    rec.add(Change {
                        path: rel.clone(),
                        kind: ChangeKind::Content,
                        before: prev.clone(),
                        structure_only,
                        tree: None,
                    });
                    self.store.release(&prev);
                    self.shadow.insert(
                        rel.clone(),
                        Shadow {
                            stamp,
                            content: new_id,
                        },
                    );
                }
                None => {
                    let (new_id, structure_only) = self.store_file(abs, *size);
                    rec.add(Change {
                        path: rel.clone(),
                        kind: ChangeKind::Create,
                        before: String::new(),
                        structure_only,
                        tree: None,
                    });
                    self.shadow.insert(
                        rel.clone(),
                        Shadow {
                            stamp,
                            content: new_id,
                        },
                    );
                }
            }
        }

        // 走査を打ち切ったときは**削除を記録しない** — 見きれなかったファイルを
        // 「消えた」と誤認するのは取り返しがつかない。
        if !truncated {
            self.record_deletes(&seen, &mut rec, at, name);
        }

        rec.end();
        let sets = rec.take();
        let changes: usize = sets.iter().map(|s| s.changes.len()).sum();
        let n = sets.len();
        self.commit(sets);
        Done::Scanned { sets: n, changes }
    }

    /// 消えたファイルを「削除の根」でまとめて 1 件ずつ記録する。
    ///
    /// ここが**入れ子の変更集合**を実際に使っている所 — 削除の切り出しは
    /// それ自体が 1 つの操作なので `begin`/`end` で包むが、外側のコマンドが
    /// 開いているので畳まれて 1 リビジョンになる。
    fn record_deletes(&mut self, seen: &HashSet<String>, rec: &mut Recorder, at: i64, name: &str) {
        let missing: Vec<String> = self
            .shadow
            .keys()
            .filter(|k| !seen.contains(*k))
            .cloned()
            .collect();
        if missing.is_empty() {
            return;
        }
        let root = self.root.clone();
        let exists = |rel: &str| root.join(rel_to_os(rel)).exists();
        let groups = delete_roots(&missing, &exists);
        for (droot, files) in groups {
            rec.begin(name, at);
            let pairs: Vec<(String, String)> = files
                .iter()
                .map(|f| {
                    let id = self
                        .shadow
                        .get(f)
                        .map(|s| s.content.clone())
                        .unwrap_or_default();
                    (f.clone(), id)
                })
                .collect();
            let tree = build_tree(&droot, &pairs);
            // 写しが参照の持ち主になる。影の索引の分はこの後で手放す。
            let mut ids = Vec::new();
            tree_ids(&tree, &mut ids);
            for id in &ids {
                self.store.acquire(id);
            }
            let single = pairs.len() == 1 && pairs[0].0 == droot;
            rec.add(Change {
                path: droot.clone(),
                kind: ChangeKind::Delete,
                before: if single {
                    pairs[0].1.clone()
                } else {
                    String::new()
                },
                structure_only: false,
                tree: Some(tree),
            });
            for f in &files {
                if let Some(s) = self.shadow.remove(f) {
                    self.store.release(&s.content);
                }
            }
            rec.end();
        }
    }

    /// 1 ファイルを倉庫へ入れる。戻り値は `(内容 ID, 構造だけか)`。
    ///
    /// バイナリと上限超過は**構造だけ**記録する (IntelliJ もバイナリは
    /// 構造だけを版管理する)。中身を持たないので ID は空になる。
    fn store_file(&mut self, abs: &Path, size: u64) -> (String, bool) {
        if size > MAX_BLOB_BYTES {
            return (String::new(), true);
        }
        let Ok(bytes) = std::fs::read(abs) else {
            return (String::new(), true);
        };
        let head = &bytes[..bytes.len().min(8192)];
        if crate::preview::looks_binary(head) {
            return (String::new(), true);
        }
        match self.store.put(&bytes) {
            Ok(id) => (id, false),
            Err(_) => (String::new(), true),
        }
    }

    /// ワークスペースを歩く。戻り値は `(見つけた物, 打ち切ったか)`。
    fn walk(&mut self) -> (Vec<(String, PathBuf, u64, i64)>, bool) {
        let root = self.root.clone();
        // 自分の保存先とアプリの状態ディレクトリは絶対に版管理しない。
        let state_dir = crate::config::zaivern_dir();
        let mut out: Vec<(String, PathBuf, u64, i64)> = Vec::new();
        let mut stack = vec![root.clone()];
        let mut truncated = false;
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                let Ok(ft) = e.file_type() else { continue };
                // シンボリックリンクは辿らない (循環と、リンク先の二重記録を避ける)。
                if ft.is_symlink() || p.starts_with(&state_dir) {
                    continue;
                }
                if ft.is_dir() {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    if name == ".git" || self.ignorer.is_ignored(&root, &p, true) {
                        continue;
                    }
                    stack.push(p);
                    continue;
                }
                if !ft.is_file() || self.ignorer.is_ignored(&root, &p, false) {
                    continue;
                }
                let Some(rel) = crate::ignore::rel_slash(&root, &p) else {
                    continue;
                };
                let Ok(md) = e.metadata() else { continue };
                out.push((rel, p, md.len(), mtime_ms(&md)));
                if out.len() >= MAX_FILES {
                    truncated = true;
                    return (out, truncated);
                }
            }
        }
        (out, truncated)
    }

    // ── 書き出しと掃除 ──────────────────────────────────────

    /// 変更集合をログへ足し、保持期間から外れた分を捨てる。
    fn commit(&mut self, sets: Vec<ChangeSet>) {
        if sets.is_empty() {
            // 空の変更集合は捨てられている。索引のスタンプ更新だけ残す。
            self.write_shadow();
            self.store.save();
            return;
        }
        self.log.extend(sets);
        self.purge();
        self.write_log();
        self.write_shadow();
        self.store.save();
    }

    /// 活動時間から外れた変更集合を落とし、参照を解放する。
    fn purge(&mut self) {
        let times: Vec<i64> = self.log.iter().map(|s| s.at).collect();
        let from = purge_from(&times, self.ret.period_ms, self.ret.gap_ms);
        if from == 0 {
            return;
        }
        let dropped: Vec<ChangeSet> = self.log.drain(..from).collect();
        for s in &dropped {
            for c in &s.changes {
                for id in refs_of(c) {
                    self.store.release(&id);
                }
            }
        }
    }

    // ── 読み出しと復元 ──────────────────────────────────────

    fn text_of(&self, st: &PathState, path: &str) -> Result<String, String> {
        match st {
            PathState::Content(id) => {
                let b = self
                    .store
                    .read(id)
                    .ok_or_else(|| tr("その時点の内容は保持期間を過ぎています"))?;
                Ok(clip_text(&b))
            }
            PathState::Absent => Ok(String::new()),
            PathState::Unchanged => {
                let abs = self.root.join(rel_to_os(path));
                match std::fs::read(&abs) {
                    Ok(b) => Ok(clip_text(&b)),
                    Err(_) => Ok(String::new()),
                }
            }
        }
    }

    fn diff(&self, idx: usize, path: &str) -> Done {
        if idx >= self.log.len() {
            return Done::Failed(tr("このリビジョンはもうありません"));
        }
        let st = content_before(&self.log, idx, path);
        let old = match self.text_of(&st, path) {
            Ok(t) => t,
            Err(e) => return Done::Failed(e),
        };
        let abs = self.root.join(rel_to_os(path));
        let new = std::fs::read(&abs)
            .map(|b| clip_text(&b))
            .unwrap_or_default();
        Done::Diff {
            title: self.log[idx].name.clone(),
            path: path.to_string(),
            old,
            new,
        }
    }

    /// 1 パスを `idx` の直前へ戻す。フォルダなら写しごと書き戻す。
    fn restore(&mut self, idx: usize, path: &str) -> Done {
        if idx >= self.log.len() {
            return Done::Failed(tr("このリビジョンはもうありません"));
        }
        // **「何を書くか」を先に確定させる。** この後の「復元前」の取り込みで
        // 保持期間の掃除が走ると `self.log` の先頭が落ちて添字がずれるので、
        // 添字を跨いで持ち越してはいけない (別のリビジョンへ戻してしまう)。
        let sub = self.log[idx]
            .changes
            .iter()
            .find(|c| c.path == path && c.kind == ChangeKind::Delete)
            .and_then(|c| c.tree.clone())
            .filter(|t| t.dir);
        let st = content_before(&self.log, idx, path);
        // 「今」を残す。戻したあとに戻れるようにするのが先。
        self.scan(&tr("復元前"));
        if let Some(tree) = sub {
            let base = self.root.join(rel_to_os(path));
            let mut written = 0usize;
            let mut kept = 0usize;
            self.write_tree(&tree, &base, &mut written, &mut kept);
            self.scan(&tr("フォルダを復元"));
            return Done::Restored { written, kept };
        }
        let (written, kept) = match self.write_state(&st, path) {
            Ok(v) => v,
            Err(e) => return Done::Failed(e),
        };
        self.scan(&tr("復元"));
        Done::Restored { written, kept }
    }

    /// `idx` 以降に触れた物をまとめて `idx` の直前へ戻す。
    fn revert(&mut self, idx: usize, prefix: &str) -> Done {
        if idx >= self.log.len() {
            return Done::Failed(tr("このリビジョンはもうありません"));
        }
        // 復元と同じ理由で、書く物は取り込みの**前**に確定させる。
        let mut paths: Vec<String> = Vec::new();
        let mut trees: Vec<(String, Entry)> = Vec::new();
        for s in self.log.iter().skip(idx) {
            for c in &s.changes {
                if !prefix.is_empty()
                    && c.path != prefix
                    && !is_ancestor(prefix, &c.path)
                    && !is_ancestor(&c.path, prefix)
                {
                    continue;
                }
                match (&c.kind, &c.tree) {
                    (ChangeKind::Delete, Some(t)) if t.dir => {
                        if !trees.iter().any(|(p, _)| p == &c.path) {
                            trees.push((c.path.clone(), t.clone()));
                        }
                    }
                    _ => {
                        if !paths.contains(&c.path) {
                            paths.push(c.path.clone());
                        }
                    }
                }
            }
        }
        let states: Vec<(String, PathState)> = paths
            .iter()
            .map(|p| (p.clone(), content_before(&self.log, idx, p)))
            .collect();
        self.scan(&tr("復元前"));
        let mut written = 0usize;
        let mut kept = 0usize;
        for (p, t) in &trees {
            let base = self.root.join(rel_to_os(p));
            self.write_tree(t, &base, &mut written, &mut kept);
        }
        for (p, st) in &states {
            match self.write_state(st, p) {
                Ok((w, k)) => {
                    written += w;
                    kept += k;
                }
                Err(_) => kept += 1,
            }
        }
        self.scan(&tr("この時点へ戻した"));
        Done::Restored { written, kept }
    }

    /// 1 パスをその時点の姿へ。**「無かった物」は消さない** (件数だけ返す)。
    ///
    /// `checkpoint.rs` と同じ方針。消す方向は取り返しがつかないので持たない。
    fn write_state(&self, st: &PathState, path: &str) -> Result<(usize, usize), String> {
        match st {
            PathState::Unchanged => Ok((0, 0)),
            PathState::Absent => Ok((0, 1)),
            PathState::Content(id) => {
                let b = self
                    .store
                    .read(id)
                    .ok_or_else(|| tr("その時点の内容は保持期間を過ぎています"))?;
                let abs = self.root.join(rel_to_os(path));
                if let Some(d) = abs.parent() {
                    std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
                }
                std::fs::write(&abs, &b).map_err(|e| e.to_string())?;
                Ok((1, 0))
            }
        }
    }

    /// 写しを実体へ書き戻す (フォルダごとの復元)。
    fn write_tree(&self, e: &Entry, at: &Path, written: &mut usize, kept: &mut usize) {
        if e.dir {
            if std::fs::create_dir_all(at).is_err() {
                *kept += 1;
                return;
            }
            for c in &e.children {
                self.write_tree(c, &at.join(&c.name), written, kept);
            }
            return;
        }
        // 既にある物は上書きしない — 復元の巻き添えで今の作業を潰さない。
        if at.exists() {
            *kept += 1;
            return;
        }
        match self.store.read(&e.content) {
            Some(b) => {
                if let Some(d) = at.parent() {
                    std::fs::create_dir_all(d).ok();
                }
                if std::fs::write(at, &b).is_ok() {
                    *written += 1;
                } else {
                    *kept += 1;
                }
            }
            None => *kept += 1,
        }
    }

    fn label(&mut self, name: &str) -> Done {
        if self.readonly {
            return Done::Failed(tr(
                "この履歴は新しい版で書かれています。読むだけにして書き換えません",
            ));
        }
        // ラベルの直前に今の姿を取り込む (「ラベルまで戻す」が意味を持つように)。
        self.scan(name);
        let mut rec = Recorder::new();
        rec.label(name, now_ms());
        self.commit(rec.take());
        Done::Loaded(self.log.clone())
    }
}

/// 裏のスレッドの本体。**何も無い間は `recv()` で完全に眠る。**
fn run(mut e: Engine, rx: Receiver<Msg>, tx: Sender<Done>, ctx: egui::Context) {
    let mut due: Option<Instant> = None;
    // 直近の予約が「一覧も欲しい」と言っていたか。
    let mut want_log_after = false;
    loop {
        let msg = match due {
            None => rx.recv().map_err(|_| ()),
            Some(t) => {
                let left = FLUSH_DELAY.saturating_sub(t.elapsed());
                match rx.recv_timeout(left) {
                    Ok(m) => Ok(m),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        due = None;
                        let name = take_name(&mut e.pending);
                        let done = e.scan(&name);
                        if tx.send(done).is_err() {
                            return;
                        }
                        if want_log_after && tx.send(Done::Loaded(e.log.clone())).is_err() {
                            return;
                        }
                        ctx.request_repaint();
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => Err(()),
                }
            }
        };
        let Ok(msg) = msg else { return };
        let done = match msg {
            Msg::Note {
                name,
                now,
                want_log,
            } => {
                e.pending = Some((name, now_ms()));
                want_log_after = want_log;
                if !now {
                    due = Some(Instant::now());
                    continue;
                }
                due = None;
                let name = take_name(&mut e.pending);
                e.scan(&name)
            }
            Msg::Load => {
                let name = take_name(&mut e.pending);
                e.scan(&name);
                Done::Loaded(e.log.clone())
            }
            Msg::Label(name) => e.label(&name),
            Msg::Diff { idx, path } => e.diff(idx, &path),
            Msg::Restore { idx, path } => e.restore(idx, &path),
            Msg::Revert { idx, prefix } => e.revert(idx, &prefix),
        };
        // 復元は必ず一覧が変わる。走査は**一覧を開いているときだけ**送る。
        let follow = matches!(done, Done::Restored { .. })
            || (want_log_after && matches!(done, Done::Scanned { .. }));
        if tx.send(done).is_err() {
            return;
        }
        if follow && tx.send(Done::Loaded(e.log.clone())).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

/// 予約されたコマンド名を取り出す。古すぎるものは「外部変更」に落とす
/// (30 分前の「保存」を今の変更の名前にしない)。
fn take_name(pending: &mut Option<(String, i64)>) -> String {
    match pending.take() {
        Some((n, at)) if now_ms().saturating_sub(at) <= NAME_GRACE_MS => n,
        _ => tr("外部変更"),
    }
}

// ══════════════════════════════════════════════════════════════════
//  小さな道具
// ══════════════════════════════════════════════════════════════════

/// Unix ミリ秒。時計が epoch より前でも落とさない。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn mtime_ms(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `/` 区切りの相対パスを OS のパスへ。Windows でも同じ結果になるよう
/// 要素へ分解してから組み直す (`\` を直書きしない)。
fn rel_to_os(rel: &str) -> PathBuf {
    let mut p = PathBuf::new();
    for part in rel.split('/').filter(|s| !s.is_empty()) {
        p.push(part);
    }
    p
}

/// 差分に載せる本文へ。上限で切り、断りを添える。
fn clip_text(bytes: &[u8]) -> String {
    if bytes.len() <= MAX_DIFF_BYTES {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    // UTF-8 の継続バイト (0b10xxxxxx) の途中で切らない。`&[u8]` には
    // `is_char_boundary` が無いので先頭ビットで判定する。
    let mut cut = MAX_DIFF_BYTES;
    while cut > 0 && bytes[cut] & 0b1100_0000 == 0b1000_0000 {
        cut -= 1;
    }
    let mut s = String::from_utf8_lossy(&bytes[..cut]).into_owned();
    s.push('\n');
    s.push_str(&tr("… (大きいのでここまで)"));
    s.push('\n');
    s
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    // 同じディレクトリへ置く (別ボリュームだと rename が失敗するため)。
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    // Windows の rename は既存を置き換える (MOVEFILE_REPLACE_EXISTING)。
    std::fs::rename(&tmp, path)
}

/// 保存先: `~/.zaivern/local_history/<ワークスペースキー>/`。
///
/// パスを直書きせず [`crate::config::zaivern_dir`] から導く。キーの作り方は
/// [`crate::history::workspace_key`] と共通 (同じフォルダは同じキーへ寄る)。
fn store_dir(root: &Path) -> PathBuf {
    crate::config::zaivern_dir()
        .join("local_history")
        .join(crate::history::workspace_key(root))
}

// ══════════════════════════════════════════════════════════════════
//  UI から使う状態
// ══════════════════════════════════════════════════════════════════

/// 一覧の絞り込み。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Scope {
    /// このファイル / フォルダだけ (`/` 区切りの相対パス)。
    Path(String),
    /// プロジェクト全体。
    Project,
}

/// ローカルヒストリの UI 側の顔。**ここから `std::fs` は 1 回も呼ばない。**
pub struct LocalHistory {
    root: PathBuf,
    tx: Option<Sender<Msg>>,
    rx: Option<Receiver<Done>>,
    /// 裏のスレッドを起こすときに要る再描画ハンドル。毎フレーム
    /// [`LocalHistory::attach`] で受け取る (保存の記録は ctx を持たない
    /// `save_buffer_to` から呼ばれるため、こちら側で持っておく)。
    ctx: Option<egui::Context>,
    /// 走行中の依頼数 (スピナーと二重起動の抑止)。
    inflight: usize,
    /// 記録が有効か (設定)。
    enabled: bool,
    /// 保持ポリシー。**設定の読み込みは UI スレッドでやらない**ので、
    /// app が持っている `Config` から渡してもらう。
    ret: Retention,
    /// `.gitignore` を尊重するか (設定)。
    gitignore: bool,
    /// 一覧を開いているか。
    pub open: bool,
    scope: Scope,
    /// 直近に開いていたファイルの相対パス (「このファイル」に戻れるように)。
    file: Option<String>,
    log: Vec<ChangeSet>,
    revs: Vec<Revision>,
    selected: usize,
    sel_path: usize,
    status: String,
    label_input: String,
    label_open: bool,
    /// 破壊的操作の確認待ち `(リビジョン添字, パス, まとめて戻すか)`。
    confirm: Option<(usize, String, bool)>,
}

impl LocalHistory {
    /// ワークスペースと設定から作る。**裏のスレッドはまだ起こさない**
    /// (最初の記録要求まで 1 スレッドも増やさない)。
    pub fn new(root: PathBuf, cfg: &crate::config::Config) -> Self {
        Self {
            root,
            tx: None,
            rx: None,
            ctx: None,
            inflight: 0,
            enabled: cfg.local_history,
            ret: Retention::from_config(cfg),
            gitignore: cfg.respect_gitignore,
            open: false,
            scope: Scope::Project,
            file: None,
            log: Vec::new(),
            revs: Vec::new(),
            selected: 0,
            sel_path: 0,
            status: String::new(),
            label_input: String::new(),
            label_open: false,
            confirm: None,
        }
    }

    /// ワークスペースが変わった。前のスレッドは送信口を落とせば終わる。
    pub fn set_workspace(&mut self, root: PathBuf, cfg: &crate::config::Config) {
        self.enabled = cfg.local_history;
        self.ret = Retention::from_config(cfg);
        self.gitignore = cfg.respect_gitignore;
        if self.root == root && self.tx.is_some() {
            return;
        }
        self.root = root;
        self.tx = None;
        self.rx = None;
        self.inflight = 0;
        self.log.clear();
        self.revs.clear();
        self.selected = 0;
        self.sel_path = 0;
        self.status.clear();
        self.confirm = None;
        self.scope = Scope::Project;
        self.file = None;
    }

    /// 毎フレーム呼ぶ。再描画ハンドルを 1 回だけ受け取る。
    ///
    /// 保存の記録は `egui::Context` を持たない経路から来るので、
    /// ここで受けた物を使い回す (`checkpoint_pending` と同じ問題への別解で、
    /// app 側にフィールドを増やさずに済む)。
    pub fn attach(&mut self, ctx: &egui::Context) {
        if self.ctx.is_none() {
            self.ctx = Some(ctx.clone());
        }
    }

    /// 保存・整形など「名前の付いた操作」の後に呼ぶ。取り込みは**遅延**して
    /// 走る (連続保存で何度も歩き回らない)。
    pub fn note(&mut self, name: &str) {
        let want_log = self.open;
        self.send(Msg::Note {
            name: name.to_string(),
            now: false,
            want_log,
        });
    }

    /// エージェントのターン境界など「今すぐ 1 枚残したい」ときに呼ぶ。
    ///
    /// **これが `checkpoint.rs` との継ぎ目**。ファイルシステム側で撮るので、
    /// エージェントの shell が書いた変更 (`rm -rf` を含む) も入る。
    pub fn snapshot(&mut self, name: &str) {
        let want_log = self.open;
        self.send(Msg::Note {
            name: name.to_string(),
            now: true,
            want_log,
        });
    }

    /// 一覧を開く。`path` があればそのファイルに絞る。
    pub fn open_for(&mut self, path: Option<&Path>, ctx: &egui::Context) {
        self.attach(ctx);
        self.open = true;
        self.file = path.and_then(|p| crate::ignore::rel_slash(&self.root, p));
        self.scope = match &self.file {
            Some(p) => Scope::Path(p.clone()),
            None => Scope::Project,
        };
        self.send(Msg::Load);
    }

    /// ラベル入力を開いた状態で一覧を出す (パレットの「ラベルを付ける」)。
    pub fn open_label(&mut self, ctx: &egui::Context) {
        self.open_for(None, ctx);
        self.label_open = true;
    }

    fn send(&mut self, msg: Msg) {
        if !self.enabled {
            self.status = tr("ローカルヒストリは設定で無効になっています");
            return;
        }
        if self.tx.is_none() {
            self.spawn();
        }
        let Some(tx) = &self.tx else { return };
        if tx.send(msg).is_ok() {
            self.inflight += 1;
        } else {
            // スレッドが落ちていた。次の要求で起こし直す。
            self.tx = None;
            self.rx = None;
        }
    }

    fn spawn(&mut self) {
        let Some(ctx) = self.ctx.clone() else { return };
        let (mtx, mrx) = mpsc::channel::<Msg>();
        let (dtx, drx) = mpsc::channel::<Done>();
        let root = self.root.clone();
        // **保存先の導出も裏で**。`store_dir` は canonicalize でディスクを触る。
        let ret = self.ret;
        let gitignore = self.gitignore;
        let spawned = std::thread::Builder::new()
            .name("zv-localhistory".into())
            .spawn(move || {
                let dir = store_dir(&root);
                if std::fs::create_dir_all(&dir).is_err() {
                    return;
                }
                let e = Engine::new(root, dir, ret, gitignore);
                run(e, mrx, dtx, ctx);
            });
        if spawned.is_ok() {
            self.tx = Some(mtx);
            self.rx = Some(drx);
        } else {
            self.status = tr("スレッドを起動できませんでした");
        }
    }

    /// 走行中か。
    fn busy(&self) -> bool {
        self.inflight > 0
    }

    /// 裏のスレッドの結果を回収する。**待たない**。
    ///
    /// 表示だけで済む物はここで畳み、app が扱う物 ([`Done::Diff`]) を返す。
    pub fn poll(&mut self) -> Option<Done> {
        let rx = self.rx.as_ref()?;
        let done = match rx.try_recv() {
            Ok(d) => d,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.rx = None;
                self.tx = None;
                self.inflight = 0;
                return None;
            }
        };
        self.inflight = self.inflight.saturating_sub(1);
        match &done {
            Done::Scanned { sets, changes } => {
                if *sets > 0 {
                    self.status = trf(
                        "{n} 件の変更を取り込みました",
                        &[("n", changes.to_string())],
                    );
                }
            }
            Done::Loaded(v) => {
                self.log = v.clone();
                self.rebuild();
            }
            Done::Restored { written, kept } => {
                self.status = trf(
                    "{n} 件を書き戻しました (その時点に無かった {k} 件はそのまま)",
                    &[("n", written.to_string()), ("k", kept.to_string())],
                );
            }
            Done::Diff { .. } => self.status.clear(),
            Done::Failed(e) => self.status = e.clone(),
        }
        Some(done)
    }

    /// 絞り込みからリビジョン一覧を作り直す (純関数を呼ぶだけ)。
    fn rebuild(&mut self) {
        let prefix = match &self.scope {
            Scope::Path(p) => p.clone(),
            Scope::Project => String::new(),
        };
        self.revs = revisions(&self.log, &prefix);
        self.selected = self.selected.min(self.revs.len().saturating_sub(1));
        self.sel_path = 0;
    }

    // ── 描画 ────────────────────────────────────────────────

    /// 一覧ウィンドウ。**閉じている間は 1 ピクセルも描かず、再描画も求めない。**
    pub fn ui(&mut self, ctx: &egui::Context) {
        self.attach(ctx);
        if !self.open {
            return;
        }
        let mut open = self.open;
        let screen = ctx.screen_rect();
        egui::Window::new(tr("🕰 ローカルヒストリ"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width((screen.width() * 0.72).clamp(420.0, 900.0))
            .default_height((screen.height() * 0.62).clamp(280.0, 620.0))
            .max_width((screen.width() - 32.0).max(280.0))
            .max_height((screen.height() - 32.0).max(200.0))
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| self.body(ui));
        self.open = open;
        if !self.open {
            self.confirm = None;
            self.label_open = false;
        }
    }

    fn body(&mut self, ui: &mut egui::Ui) {
        self.header(ui);
        if !self.enabled {
            self.empty_card(ui, tr("ローカルヒストリは設定で無効になっています"));
            return;
        }
        if let Some((idx, path, all)) = self.confirm.clone() {
            self.confirm_body(ui, idx, &path, all);
            return;
        }
        if self.revs.is_empty() {
            self.empty_card(
                ui,
                if self.busy() {
                    tr("読み込み中…")
                } else {
                    tr("まだ履歴がありません。保存かエージェントの実行で貯まります")
                },
            );
            return;
        }

        // 割り付けは純関数で決める (テーブルテストで固定してある)。
        let area = ui.available_rect_before_wrap();
        let detail = self.revs.get(self.selected).map(|r| !r.paths.is_empty()) == Some(true);
        let (list_r, detail_r) = history_rects(area, detail);
        let mut list_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(list_r)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        list_ui.set_clip_rect(list_r);
        self.list_ui(&mut list_ui);
        if let Some(dr) = detail_r {
            let mut d = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(dr)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            d.set_clip_rect(dr);
            self.detail_ui(&mut d);
        }
        ui.allocate_rect(area, egui::Sense::hover());
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let project = self.scope == Scope::Project;
            if ui
                .selectable_label(project, tr("プロジェクト全体"))
                .clicked()
                && !project
            {
                self.scope = Scope::Project;
                self.rebuild();
            }
            if let Some(f) = self.file.clone() {
                let on = matches!(&self.scope, Scope::Path(p) if *p == f);
                let short = tail(&f, 28);
                if ui
                    .selectable_label(on, short)
                    .on_hover_text(f.clone())
                    .clicked()
                    && !on
                {
                    self.scope = Scope::Path(f);
                    self.rebuild();
                }
            }
            if ui
                .add_enabled(!self.busy(), egui::Button::new(tr("取り込む")))
                .on_hover_text(tr("今の作業ツリーを 1 枚記録します"))
                .clicked()
            {
                self.snapshot(&tr("手動で取り込み"));
            }
            if ui
                .add_enabled(!self.busy(), egui::Button::new(tr("ラベル…")))
                .on_hover_text(tr(
                    "この時点に名前を付けます (後で「ここへ戻す」ができます)",
                ))
                .clicked()
            {
                self.label_open = !self.label_open;
            }
            if self.busy() {
                ui.spinner();
            }
        });
        if self.label_open {
            ui.horizontal_wrapped(|ui| {
                let w = (ui.available_width() - 96.0).clamp(80.0, 320.0);
                ui.add_sized(
                    egui::vec2(w, ui.spacing().interact_size.y),
                    egui::TextEdit::singleline(&mut self.label_input)
                        .hint_text(tr("ラベル名"))
                        .id_salt("zv-lh-label"),
                );
                let ok = !self.label_input.trim().is_empty() && !self.busy();
                if ui
                    .add_enabled(ok, egui::Button::new(tr("付ける")))
                    .clicked()
                {
                    let name = self.label_input.trim().to_string();
                    self.send(Msg::Label(name));
                    self.label_input.clear();
                    self.label_open = false;
                }
            });
        }
        if !self.status.is_empty() {
            let (w, cw) = (ui.available_width(), glyph_w(ui));
            ui.label(
                egui::RichText::new(elide(&self.status, w, cw))
                    .small()
                    .weak(),
            )
            .on_hover_text(self.status.clone());
        }
        ui.separator();
    }

    fn empty_card(&self, ui: &mut egui::Ui, text: String) {
        // 空状態は利用可能領域の**中央**に 1 枚だけ。高さは確保しない。
        let area = ui.available_rect_before_wrap();
        let mut c = ui.new_child(egui::UiBuilder::new().max_rect(area).layout(
            egui::Layout::centered_and_justified(egui::Direction::TopDown),
        ));
        c.label(egui::RichText::new(text).weak());
        ui.allocate_rect(area, egui::Sense::hover());
    }

    fn list_ui(&mut self, ui: &mut egui::Ui) {
        let now = crate::git::unix_now();
        let avail = ui.available_width();
        let cw = glyph_w(ui);
        let row_h = ui.spacing().interact_size.y;
        let mut clicked: Option<usize> = None;
        let selected = self.selected;
        {
            let revs = &self.revs;
            egui::ScrollArea::vertical()
                .id_salt("zv-lh-list")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (i, r) in revs.iter().enumerate() {
                        let full = list_label(r, now);
                        let resp = ui.add_sized(
                            egui::vec2(avail.max(80.0), row_h),
                            egui::SelectableLabel::new(selected == i, elide(&full, avail, cw)),
                        );
                        if resp.on_hover_text(full).clicked() {
                            clicked = Some(i);
                        }
                    }
                });
        }
        if let Some(i) = clicked {
            self.selected = i;
            self.sel_path = 0;
        }
    }

    fn detail_ui(&mut self, ui: &mut egui::Ui) {
        let Some(rev) = self.revs.get(self.selected).cloned() else {
            return;
        };
        let avail = ui.available_width();
        let cw = glyph_w(ui);
        ui.label(
            egui::RichText::new(elide(&rev.name, avail, cw))
                .strong()
                .small(),
        )
        .on_hover_text(rev.name.clone());
        let mut clicked: Option<usize> = None;
        let sel = self.sel_path.min(rev.paths.len().saturating_sub(1));
        {
            let paths = &rev.paths;
            egui::ScrollArea::vertical()
                .id_salt("zv-lh-paths")
                .auto_shrink([false, true])
                .max_height((ui.available_height() - 40.0).max(40.0))
                .show(ui, |ui| {
                    for (i, p) in paths.iter().take(MAX_SHOWN_PATHS).enumerate() {
                        if ui
                            .selectable_label(sel == i, elide(p, avail, cw))
                            .on_hover_text(p.clone())
                            .clicked()
                        {
                            clicked = Some(i);
                        }
                    }
                    if paths.len() > MAX_SHOWN_PATHS {
                        ui.label(
                            egui::RichText::new(trf(
                                "ほか {n} 件",
                                &[("n", (paths.len() - MAX_SHOWN_PATHS).to_string())],
                            ))
                            .small()
                            .weak(),
                        );
                    }
                });
        }
        if let Some(i) = clicked {
            self.sel_path = i;
        }
        let path = rev.paths.get(sel).cloned().unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            let on = !path.is_empty() && !self.busy();
            if ui
                .add_enabled(on, egui::Button::new(tr("差分")))
                .on_hover_text(tr("この時点と今を比べます"))
                .clicked()
            {
                self.send(Msg::Diff {
                    idx: rev.index,
                    path: path.clone(),
                });
            }
            let restore_label = if rev.deleted {
                tr("復元…")
            } else {
                tr("戻す…")
            };
            if ui
                .add_enabled(on, egui::Button::new(restore_label))
                .on_hover_text(tr("選んだパスをこの時点の姿に戻します"))
                .clicked()
            {
                self.confirm = Some((rev.index, path.clone(), false));
            }
            if ui
                .add_enabled(!self.busy(), egui::Button::new(tr("以降を全部…")))
                .on_hover_text(tr("この時点以降に触れた物をまとめて戻します"))
                .clicked()
            {
                self.confirm = Some((rev.index, String::new(), true));
            }
        });
    }

    fn confirm_body(&mut self, ui: &mut egui::Ui, idx: usize, path: &str, all: bool) {
        let now = crate::git::unix_now();
        let title = self
            .revs
            .iter()
            .find(|r| r.index == idx)
            .map(|r| list_label(r, now))
            .unwrap_or_default();
        ui.label(egui::RichText::new(tr("この時点へ戻しますか?")).strong());
        let (w, cw) = (ui.available_width(), glyph_w(ui));
        ui.label(elide(&title, w, cw)).on_hover_text(title);
        if !all {
            ui.label(elide(path, w, cw)).on_hover_text(path.to_string());
        }
        ui.label(
            egui::RichText::new(tr(
                "戻す直前に「今」を自動で取り込むので、戻した後に戻れます。その時点に無かったファイルは消しません。",
            ))
            .small()
            .weak(),
        );
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!self.busy(), egui::Button::new(tr("戻す")))
                .clicked()
            {
                let prefix = match &self.scope {
                    Scope::Path(p) => p.clone(),
                    Scope::Project => String::new(),
                };
                let msg = if all {
                    Msg::Revert { idx, prefix }
                } else {
                    Msg::Restore {
                        idx,
                        path: path.to_string(),
                    }
                };
                self.send(msg);
                self.confirm = None;
            }
            if ui.button(tr("やめる")).clicked() {
                self.confirm = None;
            }
            if self.busy() {
                ui.spinner();
            }
        });
    }
}

/// 一覧の 1 行の文言。**時刻とコマンド名**が主役 (JetBrains の一覧が
/// 「活動ログ」に見えるのはここが命名されているから)。
fn list_label(r: &Revision, now_secs: i64) -> String {
    let when = crate::git::relative_time(r.at / 1000, now_secs);
    if r.label {
        return trf(
            "🔖 {when} · {name}",
            &[("when", when), ("name", r.name.clone())],
        );
    }
    trf(
        "{when} · {name} · {n} 件",
        &[
            ("when", when),
            ("name", r.name.clone()),
            ("n", r.paths.len().to_string()),
        ],
    )
}

/// 長いパスの**末尾**を残して縮める (どのファイルかは末尾で分かる)。
fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let skip = n - max.saturating_sub(1);
    let mut out = String::from("…");
    out.extend(s.chars().skip(skip));
    out
}

/// いま描いている本文フォントの 1 桁ぶんの幅。**決め打ちにしない** —
/// UI 拡大率とフォント設定で変わるので、毎回測る (`checkpoint.rs` と同じ)。
fn glyph_w(ui: &egui::Ui) -> f32 {
    ui.fonts(|f| f.glyph_width(&egui::TextStyle::Body.resolve(ui.style()), 'M'))
}

/// 省略しても意味が残る最低桁数。これを下回るほど狭いときは、
/// 短く切るより溢れる方がまし。
const MIN_LABEL_COLS: usize = 10;

/// 可用幅に入る文字数へ縮める。**CJK は 1 文字 2 桁ぶん**食うので、
/// 桁数の半分で見積もって溢れない側へ倒す (全文はホバーで読める)。
fn elide(s: &str, avail_w: f32, char_w: f32) -> String {
    if !avail_w.is_finite() || avail_w <= 0.0 || !(char_w > 0.0) {
        return s.to_string();
    }
    let cols = ((avail_w / char_w / 2.0) as usize).max(MIN_LABEL_COLS);
    if s.chars().count() <= cols {
        return s.to_string();
    }
    let mut out: String = s.chars().take(cols.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    fn cfg_with(days: u32, hours: u32, on: bool) -> crate::config::Config {
        let mut c = crate::config::Config::default();
        c.local_history = on;
        c.local_history_days = days;
        c.local_history_gap_hours = hours;
        c
    }

    // ── 変更集合の入れ子 ────────────────────────────────────

    #[test]
    fn 入れ子のbegin_endは畳まれて外側の名前で1件になる() {
        let mut r = Recorder::new();
        r.begin("名前の変更", 100);
        r.begin("整形", 100); // 入れ子: 名前は外側が勝つ
        r.add(Change {
            path: "a.rs".into(),
            ..Default::default()
        });
        r.end(); // 内側: まだ書き出さない
        assert!(r.take().is_empty(), "内側の end で書き出してはいけない");
        r.add(Change {
            path: "b.rs".into(),
            ..Default::default()
        });
        r.end(); // 外側でだけ書き出す
        let out = r.take();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "名前の変更");
        assert_eq!(out[0].changes.len(), 2);
    }

    #[test]
    fn 空の変更集合は捨てられラベルは残る() {
        // (積んだ変更の数, 期待する件数)
        let table: &[(usize, usize)] = &[(0, 0), (1, 1), (3, 1)];
        for &(n, want) in table {
            let mut r = Recorder::new();
            r.begin("保存", 1);
            for i in 0..n {
                r.add(Change {
                    path: format!("f{i}.rs"),
                    ..Default::default()
                });
            }
            r.end();
            assert_eq!(r.take().len(), want, "変更 {n} 件のとき");
        }
        // ラベルは中身が空でも残る (印そのものが中身)
        let mut r = Recorder::new();
        r.label("リリース前", 5);
        let out = r.take();
        assert_eq!(out.len(), 1);
        assert!(out[0].label && out[0].changes.is_empty());
    }

    #[test]
    fn 開いていないrecorderへのaddは捨てられる() {
        let mut r = Recorder::new();
        assert!(!r.add(Change::default()), "開く前の add は通さない");
        r.end(); // 深さ 0 で end してもパニックしない
        assert!(r.take().is_empty());
    }

    // ── 修正スタンプ ────────────────────────────────────────

    #[test]
    fn 修正スタンプが同じなら変更として拾わない() {
        // (サイズ, 更新時刻) の表。片方だけ変わっても別スタンプになること。
        let table: &[(u64, i64, u64, i64, bool)] = &[
            (10, 1000, 10, 1000, false), // 同一 → 変化なし
            (10, 1000, 11, 1000, true),  // サイズだけ違う
            (10, 1000, 10, 1001, true),  // 時刻だけ違う
            (0, 0, 0, 0, false),         // 空ファイル同士
        ];
        for &(s1, m1, s2, m2, want) in table {
            let sh = Shadow {
                stamp: stamp_of(s1, m1),
                content: "x".into(),
            };
            assert_eq!(
                sh.stamp_changed(stamp_of(s2, m2)),
                want,
                "({s1},{m1}) → ({s2},{m2})"
            );
        }
    }

    // ── 保持 (活動時間) ─────────────────────────────────────

    #[test]
    fn 保持は活動時間で数え12時間超の空白は1msになる() {
        const H: i64 = 60 * 60 * 1000;
        let gap = 12 * H;
        let period = 5000; // 活動 5 秒ぶんだけ持つ
                           // (時刻列, 期待する「残す先頭の添字」, 説明)
        let table: &[(&[i64], usize, &str)] = &[
            (&[], 0, "空"),
            (&[100], 0, "1 件なら必ず残る"),
            (&[0, 1000, 2000, 3000], 0, "合計 3 秒 → 全部残る"),
            (&[0, 3000, 6000, 9000], 2, "合計 9 秒 → 古い 2 件が落ちる"),
            // 12 時間ちょうどは「超えていない」ので実測どおり足す
            (&[0, 12 * H, 12 * H + 1000], 1, "ちょうど 12 時間は足される"),
            // 12 時間 + 1ms は空白 → 1ms しか食わない
            (
                &[0, 12 * H + 1, 12 * H + 2000],
                0,
                "12 時間超の空白は 1ms 扱い",
            ),
            // 1 週間離れていても予算を食わない
            (
                &[0, 7 * 24 * H, 7 * 24 * H + 1000],
                0,
                "1 週間の空白でも落ちない",
            ),
        ];
        for &(times, want, why) in table {
            assert_eq!(purge_from(times, period, gap), want, "{why}: {times:?}");
        }
    }

    #[test]
    fn 保持は逆順や同時刻でも壊れない() {
        // 同じミリ秒に複数 → 差 0 なので消えない
        assert_eq!(purge_from(&[5, 5, 5, 5], 1, 10), 0);
        // 時刻が逆行していても負にはしない (max(0))
        assert_eq!(purge_from(&[100, 50, 10], 1_000_000, 10), 0);
    }

    // ── 内容ストア (参照数) ─────────────────────────────────

    #[test]
    fn 内容は1個だけ持ち参照数が0で消える() {
        let dir = crate::test_util::unique_temp_dir("zaivern-lh-test", "store");
        let mut s = Store::new(dir.clone());
        let a = s.put(b"hello").expect("put");
        let b = s.put(b"hello").expect("put 2");
        assert_eq!(a, b, "同じ内容は同じ ID");
        assert_eq!(s.refs.get(&a).copied().unwrap_or(0), 2, "参照数が積まれる");
        let blob = s.blob_path(&a);
        assert!(blob.exists());
        s.release(&a);
        assert_eq!(s.refs.get(&a).copied().unwrap_or(0), 1);
        assert!(blob.exists(), "まだ参照が残っているので消さない");
        s.release(&a);
        assert_eq!(s.refs.get(&a).copied().unwrap_or(0), 0);
        assert!(!blob.exists(), "0 になったら blob を消す");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 内容idが衝突しても別内容を取り違えない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-lh-test", "collide");
        let mut s = Store::new(dir.clone());
        let id = s.put(b"first").expect("put");
        // 同じ ID の場所へ**別の中身**を置いて衝突を作る。
        std::fs::write(s.blob_path(&id), b"tampered").expect("tamper");
        let id2 = s.put(b"first").expect("put again");
        assert_ne!(id, id2, "中身が違うので別名になる");
        assert_eq!(s.read(&id2).as_deref(), Some(&b"first"[..]));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 参照数はファイルへ残り読み直せる() {
        let dir = crate::test_util::unique_temp_dir("zaivern-lh-test", "refs");
        let id = {
            let mut s = Store::new(dir.clone());
            let id = s.put(b"keep me").expect("put");
            s.save();
            id
        };
        let s2 = Store::new(dir.clone());
        assert_eq!(s2.refs.get(&id).copied().unwrap_or(0), 1);
        assert_eq!(s2.read(&id).as_deref(), Some(&b"keep me"[..]));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── 削除サブツリー ──────────────────────────────────────

    #[test]
    fn 消えたファイルはフォルダの根でまとまる() {
        let missing = vec![
            "src/gone/a.rs".to_string(),
            "src/gone/deep/b.rs".to_string(),
            "src/alive.rs".to_string(),
        ];
        // `src` は残っているが `src/gone` は消えた
        let exists = |p: &str| p == "src";
        let g = delete_roots(&missing, &exists);
        assert_eq!(g.len(), 2, "根は 2 つ: src/gone と src/alive.rs");
        assert_eq!(
            g.get("src/gone").map(Vec::len),
            Some(2),
            "フォルダ配下は 1 件にまとまる"
        );
        assert!(g.contains_key("src/alive.rs"), "単体削除はそのまま");
    }

    #[test]
    fn 写しは入れ子で組まれ内容idを保つ() {
        let files = vec![
            ("src/gone/a.rs".to_string(), "id-a".to_string()),
            ("src/gone/deep/b.rs".to_string(), "id-b".to_string()),
        ];
        let t = build_tree("src/gone", &files);
        assert!(t.dir && t.name == "gone");
        assert_eq!(
            tree_at(&t, "a.rs").map(|e| e.content.clone()).as_deref(),
            Some("id-a")
        );
        assert_eq!(
            tree_at(&t, "deep/b.rs")
                .map(|e| e.content.clone())
                .as_deref(),
            Some("id-b")
        );
        // ファイル 1 個の削除なら葉になる
        let one = build_tree("x.txt", &[("x.txt".to_string(), "id-x".to_string())]);
        assert!(!one.dir && one.content == "id-x");
        // 参照の数え上げ
        let mut ids = Vec::new();
        tree_ids(&t, &mut ids);
        ids.sort();
        assert_eq!(ids, vec!["id-a".to_string(), "id-b".to_string()]);
    }

    // ── 再生 ────────────────────────────────────────────────

    fn set(at: i64, name: &str, changes: Vec<Change>) -> ChangeSet {
        ChangeSet {
            at,
            name: name.into(),
            label: false,
            changes,
        }
    }

    fn content(path: &str, before: &str) -> Change {
        Change {
            path: path.into(),
            kind: ChangeKind::Content,
            before: before.into(),
            ..Default::default()
        }
    }

    #[test]
    fn リビジョンは新しい順でラベルは絞り込みでも残る() {
        let log = vec![
            set(1000, "保存", vec![content("a.rs", "v1")]),
            ChangeSet {
                at: 2000,
                name: "リリース前".into(),
                label: true,
                changes: vec![],
            },
            set(3000, "エージェント", vec![content("b.rs", "v1")]),
        ];
        let all = revisions(&log, "");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].at, 3000, "新しい順");
        let only_a = revisions(&log, "a.rs");
        // a.rs の変更 1 件 + ラベル 1 件
        assert_eq!(only_a.len(), 2);
        assert!(only_a.iter().any(|r| r.label));
        assert!(only_a.iter().any(|r| r.paths == vec!["a.rs".to_string()]));
    }

    #[test]
    fn ある時点の内容は変更前の記録から遡って決まる() {
        let log = vec![
            set(1000, "保存", vec![content("a.rs", "c1")]),
            set(2000, "保存", vec![content("a.rs", "c2")]),
            set(3000, "保存", vec![content("b.rs", "c9")]),
        ];
        // 添字 0 の直前 = c1 (いちばん古い記録)
        assert_eq!(
            content_before(&log, 0, "a.rs"),
            PathState::Content("c1".into())
        );
        // 添字 1 の直前 = c2
        assert_eq!(
            content_before(&log, 1, "a.rs"),
            PathState::Content("c2".into())
        );
        // 添字 2 以降 a.rs は触っていない = 今と同じ
        assert_eq!(content_before(&log, 2, "a.rs"), PathState::Unchanged);
        // 作成の直前は「無かった」
        let log2 = vec![set(
            1,
            "作成",
            vec![Change {
                path: "n.rs".into(),
                kind: ChangeKind::Create,
                ..Default::default()
            }],
        )];
        assert_eq!(content_before(&log2, 0, "n.rs"), PathState::Absent);
    }

    #[test]
    fn 消えたフォルダの中の1ファイルも遡れる() {
        let tree = build_tree(
            "src/gone",
            &[("src/gone/deep/b.rs".to_string(), "id-b".to_string())],
        );
        let log = vec![set(
            1,
            "削除",
            vec![Change {
                path: "src/gone".into(),
                kind: ChangeKind::Delete,
                tree: Some(tree),
                ..Default::default()
            }],
        )];
        assert_eq!(
            content_before(&log, 0, "src/gone/deep/b.rs"),
            PathState::Content("id-b".into())
        );
        // 一覧の絞り込みでも祖先の削除を拾う
        let r = revisions(&log, "src/gone/deep/b.rs");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].paths, vec!["src/gone/deep/b.rs".to_string()]);
        assert!(r[0].deleted);
    }

    // ── 版の移行 ────────────────────────────────────────────

    #[test]
    fn 古い版の記録は捨てずに読み替える() {
        // v1: 時刻は**秒**、変更に種別が無い
        let v1 = r#"{"at":1700000000,"name":"保存","label":false,"changes":[{"path":"a.rs","before":"c1"}]}"#;
        let s = migrate_line(1, v1).expect("v1 を読める");
        assert_eq!(s.at, 1_700_000_000_000, "秒 → ミリ秒");
        assert_eq!(s.name, "保存");
        assert_eq!(s.changes[0].kind, ChangeKind::Content);
        assert_eq!(s.changes[0].before, "c1");
        // 未来の版は読まない (= 呼び出し側が読み取り専用にする)
        assert!(migrate_line(FORMAT_VERSION + 1, v1).is_none());
        // 今の版はそのまま
        let cur = serde_json::to_string(&set(5, "x", vec![content("a", "b")])).expect("json");
        assert!(migrate_line(FORMAT_VERSION, &cur).is_some());
    }

    #[test]
    fn 版が古い保存を開くと移行して版が上がる() {
        let dir = crate::test_util::unique_temp_dir("zaivern-lh-test", "migrate");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).expect("ws");
        let store = dir.join("store");
        std::fs::create_dir_all(&store).expect("store");
        std::fs::write(store.join("meta.json"), br#"{"version":1}"#).expect("meta");
        std::fs::write(
            store.join("log.jsonl"),
            // CRLF も混ぜて「Windows のチェックアウトでも読める」を確かめる
            "{\"at\":1700000000,\"name\":\"保存\",\"changes\":[{\"path\":\"a.rs\",\"before\":\"c1\"}]}\r\n",
        )
        .expect("log");
        let ret = Retention::from_config(&cfg_with(5, 12, true));
        let e = Engine::new(ws, store.clone(), ret, true);
        assert!(!e.readonly);
        assert_eq!(e.log.len(), 1, "移行して読めている");
        assert_eq!(e.log[0].at, 1_700_000_000_000);
        let meta = std::fs::read_to_string(store.join("meta.json")).expect("meta 再読");
        assert!(
            meta.contains(&format!("{FORMAT_VERSION}")),
            "版が上がっている: {meta}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 未来の版は読み取り専用にして壊さない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-lh-test", "future");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).expect("ws");
        let store = dir.join("store");
        std::fs::create_dir_all(&store).expect("store");
        let future = format!(r#"{{"version":{}}}"#, FORMAT_VERSION + 1);
        std::fs::write(store.join("meta.json"), future.as_bytes()).expect("meta");
        std::fs::write(store.join("log.jsonl"), b"{}\n").expect("log");
        let ret = Retention::from_config(&cfg_with(5, 12, true));
        let mut e = Engine::new(ws, store.clone(), ret, true);
        assert!(e.readonly);
        assert!(matches!(e.scan("保存"), Done::Failed(_)), "書かせない");
        let meta = std::fs::read_to_string(store.join("meta.json")).expect("meta 再読");
        assert!(
            meta.contains(&format!("{}", FORMAT_VERSION + 1)),
            "版を書き換えない"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── 実際のツリーで通しを確かめる ────────────────────────

    /// 一時ワークスペースとエンジンを組む (**実 `~/.zaivern` には触れない**)。
    fn engine_for(tag: &str) -> (PathBuf, Engine) {
        let dir = crate::test_util::unique_temp_dir("zaivern-lh-test", tag);
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).expect("ws");
        let store = dir.join("store");
        std::fs::create_dir_all(&store).expect("store");
        let ret = Retention::from_config(&cfg_with(5, 12, true));
        // gitignore は切る (一時ディレクトリに .gitignore は無いが、
        // グローバル除外を読みに行かせない)
        let e = Engine::new(ws, store, ret, false);
        (dir, e)
    }

    /// 更新時刻を必ず動かして書く (同じ ms に 2 回書くとスタンプが変わらない)。
    fn write_file(p: &Path, body: &str) {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).expect("mkdir");
        }
        std::fs::write(p, body).expect("write");
        // ミリ秒解像度のファイルシステムでもスタンプが動くよう少し待つ。
        std::thread::sleep(std::time::Duration::from_millis(12));
    }

    #[test]
    fn 編集と削除を記録してファイルもフォルダも戻せる() {
        let (dir, mut e) = engine_for("roundtrip");
        let ws = e.root.clone();
        // 1) 最初の姿を取り込む
        write_file(&ws.join("a.txt"), "one\n");
        write_file(&ws.join("keep/b.txt"), "bee\n");
        e.scan("初回");
        // 2) 書き換える
        write_file(&ws.join("a.txt"), "two\n");
        e.scan("保存");
        assert!(
            e.log.iter().any(|s| s.name == "保存"),
            "コマンド名が一覧に残る: {:?}",
            e.log.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
        );
        // 3) フォルダごと消す
        std::fs::remove_dir_all(ws.join("keep")).expect("rm");
        e.scan("フォルダを削除");
        let del = e
            .log
            .iter()
            .position(|s| s.changes.iter().any(|c| c.kind == ChangeKind::Delete))
            .expect("削除が記録されている");
        // 4) ファイルを 1 世代戻す
        let idx = e
            .log
            .iter()
            .position(|s| s.name == "保存")
            .expect("保存の記録");
        let done = e.restore(idx, "a.txt");
        assert!(
            matches!(done, Done::Restored { written: 1, .. }),
            "{done:?}"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).expect("読める"),
            "one\n"
        );
        // 5) 消したフォルダを戻す
        let done = e.restore(del, "keep");
        assert!(
            matches!(done, Done::Restored { written: 1, .. }),
            "{done:?}"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("keep/b.txt")).expect("戻っている"),
            "bee\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 内容が同じなら記録もふくらまない() {
        let (dir, mut e) = engine_for("nodup");
        let ws = e.root.clone();
        write_file(&ws.join("a.txt"), "same\n");
        e.scan("初回");
        let n = e.log.len();
        // 更新時刻だけ動かして中身は同じ → 変更として記録しない
        write_file(&ws.join("a.txt"), "same\n");
        e.scan("保存");
        assert_eq!(e.log.len(), n, "中身が同じなら変更集合を作らない");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn バイナリは構造だけ記録して中身を持たない() {
        let (dir, mut e) = engine_for("binary");
        let ws = e.root.clone();
        std::fs::write(ws.join("blob.bin"), [0u8, 1, 2, 3, 0, 9]).expect("write");
        e.scan("初回");
        let c = e
            .log
            .iter()
            .flat_map(|s| s.changes.iter())
            .find(|c| c.path == "blob.bin")
            .expect("記録されている");
        assert!(c.structure_only, "構造だけ");
        assert!(
            e.shadow
                .get("blob.bin")
                .is_some_and(|s| s.content.is_empty()),
            "内容は持たない"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 保持期間を過ぎた記録は参照ごと消える() {
        let (dir, mut e) = engine_for("purge");
        let ws = e.root.clone();
        // 3 世代作る。**変更前の内容 v1 を握っているのは 2 番目の変更集合**
        // なので、そこが落ちて初めて blob が消える。
        write_file(&ws.join("a.txt"), "v1\n");
        e.scan("初回");
        write_file(&ws.join("a.txt"), "v2\n");
        e.scan("保存1");
        write_file(&ws.join("a.txt"), "v3\n");
        e.scan("保存2");
        assert_eq!(e.log.len(), 3, "3 世代ぶん記録されている");
        let v1 = e.log[1]
            .changes
            .iter()
            .find(|c| c.path == "a.txt")
            .map(|c| c.before.clone())
            .expect("変更前の内容がある");
        assert!(!v1.is_empty());
        let blob = e.store.blob_path(&v1);
        assert_eq!(
            e.store.refs.get(&v1).copied(),
            Some(1),
            "記録が唯一の持ち主"
        );
        assert!(blob.exists());
        // 活動時間 0 にすると、いちばん新しい 1 件以外は落ちる。
        // 時刻が同じだと差が 0 で落ちないので、古い方を過去へずらす。
        e.ret.period_ms = 0;
        e.log[0].at -= 20_000;
        e.log[1].at -= 10_000;
        e.purge();
        assert_eq!(e.log.len(), 1, "新しい 1 件だけ残る");
        assert_eq!(e.store.refs.get(&v1).copied(), None, "参照が解放されている");
        assert!(!blob.exists(), "参照が 0 になった blob は消える");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── 画面の割り付け ──────────────────────────────────────

    fn rect(w: f32, h: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(w, h))
    }

    #[test]
    fn 割り付けはどの幅でも領域に収まり重ならない() {
        // (幅, 高さ, 詳細を出すか, 2 列か)
        let table: &[(f32, f32, bool, bool)] = &[
            (900.0, 700.0, true, true),
            (1200.0, 300.0, true, true),
            (620.0, 700.0, true, true),   // 2 列の下限ちょうど
            (480.0, 700.0, true, false),  // 狭い → 縦積み
            (480.0, 160.0, true, false),  // 低すぎる → 1 列
            (900.0, 700.0, false, false), // 詳細なし → 枠を作らない
        ];
        for &(w, h, detail, two_col) in table {
            let area = rect(w, h);
            let (list, d) = history_rects(area, detail);
            assert!(area.contains_rect(list), "一覧が領域外 ({w}x{h})");
            match d {
                Some(d) => {
                    assert!(area.contains_rect(d), "詳細が領域外 ({w}x{h})");
                    assert!(
                        !list.intersects(d),
                        "一覧と詳細が重なっている ({w}x{h}): {list:?} / {d:?}"
                    );
                    assert!(list.width() > 0.0 && list.height() > 0.0);
                    assert!(d.width() > 0.0 && d.height() > 0.0);
                    assert_eq!(d.min.x > list.max.x, two_col, "列の向きが違う ({w}x{h})");
                }
                None => assert!(!two_col && (!detail || h < 200.0 || w < TWO_COL_MIN_W)),
            }
        }
    }

    #[test]
    fn 詳細が無いときは一覧が領域を全部使う() {
        let area = rect(900.0, 700.0);
        let (list, d) = history_rects(area, false);
        assert_eq!(list, area, "空の枠で高さを取らない");
        assert!(d.is_none());
    }

    // ── 表示の文言 ──────────────────────────────────────────

    #[test]
    fn 一覧の行は時刻とコマンド名を主役にする() {
        let now = 1_700_000_000;
        let r = Revision {
            index: 0,
            at: (now - 120) * 1000,
            name: "保存".into(),
            label: false,
            paths: vec!["a.rs".into(), "b.rs".into()],
            deleted: false,
        };
        let s = list_label(&r, now);
        assert!(s.contains("保存"), "コマンド名が出る: {s}");
        assert!(s.contains('2'), "件数が出る: {s}");
        let l = Revision {
            label: true,
            name: "リリース前".into(),
            paths: vec![],
            ..r
        };
        assert!(list_label(&l, now).contains("リリース前"));
    }

    #[test]
    fn 長い文言は幅に応じて省略される() {
        let long = "あ".repeat(200);
        let s = elide(&long, 200.0, 7.0);
        assert!(s.chars().count() < 200, "縮んでいる");
        assert!(s.ends_with('…'), "続きがあることを示す: {s}");
        // 幅が壊れていても落ちない
        assert_eq!(elide("x", f32::NAN, 7.0), "x");
        assert_eq!(
            elide("x", 100.0, 0.0),
            "x",
            "グリフ幅が測れなくても落ちない"
        );
        // 狭すぎても最低桁数までは残す
        assert!(elide(&long, 1.0, 7.0).chars().count() >= MIN_LABEL_COLS);
        // パスは末尾を残す
        assert!(tail("very/long/path/to/file.rs", 10).ends_with("file.rs"));
        assert_eq!(tail("short.rs", 20), "short.rs");
    }

    #[test]
    fn 相対パスはosに依らず同じ要素へ割れる() {
        assert_eq!(
            rel_to_os("a/b/c.rs"),
            PathBuf::from("a").join("b").join("c.rs")
        );
        assert_eq!(rel_to_os(""), PathBuf::new());
        assert!(is_ancestor("src", "src/a.rs"));
        assert!(!is_ancestor("src", "srcx/a.rs"));
        assert!(!is_ancestor("", "a.rs"));
        assert!(!is_ancestor("src/a.rs", "src/a.rs"));
    }

    #[test]
    fn 予約したコマンド名は古くなると外部変更へ落ちる() {
        let mut fresh = Some(("保存".to_string(), now_ms()));
        assert_eq!(take_name(&mut fresh), "保存");
        assert!(fresh.is_none(), "1 回で消える");
        let mut old = Some(("保存".to_string(), now_ms() - NAME_GRACE_MS - 1));
        assert_eq!(take_name(&mut old), tr("外部変更"));
        let mut none: Option<(String, i64)> = None;
        assert_eq!(take_name(&mut none), tr("外部変更"));
    }

    #[test]
    fn 保持設定は0や負でも壊れない() {
        let r = Retention::from_config(&cfg_with(0, 0, true));
        assert!(r.period_ms > 0 && r.gap_ms > 0, "下限で止める");
        let d = Retention::from_config(&crate::config::Config::default());
        assert!(crate::config::Config::default().local_history, "既定は有効");
        assert_eq!(d.period_ms, 5 * 24 * 60 * 60 * 1000, "既定は 5 日");
        assert_eq!(d.gap_ms, 12 * 60 * 60 * 1000, "既定は 12 時間");
    }

    #[test]
    fn 保存先は実ユーザーのzaivern配下から導かれる() {
        let p = store_dir(Path::new("."));
        assert!(p.starts_with(crate::config::zaivern_dir().join("local_history")));
        // ワークスペースが違えば別のフォルダ
        assert_ne!(store_dir(Path::new(".")), store_dir(Path::new("..")));
    }
}
