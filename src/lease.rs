//! **ファイル所有リース** — 並列エージェントの衝突を「検出」ではなく「発生させない」。
//!
//! ## なぜ要るのか (計測された空白)
//!
//! 9 種の OSS エージェント・オーケストレータを調べた調査は「**どれ 1 つとして
//! マージ衝突の処理を自動化していない**」と結論している。各ツールの答えは
//! 揃って「git worktree がファイルシステムを分離する」で、これは
//! **同じファイルへの同時書き込みを防ぐだけ**であり、
//! **2 つの worktree で同じファイルを編集する 2 人**には 1 ミリも効かない。
//!
//! - AgenticFlict (arXiv 2604.03551) は 142,000 件超のエージェント PR を測って
//!   **衝突率 27.67%**。
//! - Cursor の swarm 実験は **2 時間で 7 万件超のマージ衝突**を出して中断。
//!
//! CLAUDE.md 設計原則 5 の「セッションの所有権はアトミックに主張し、競合したら
//! fail-closed にする」を、**ファイル単位**へ下ろしたのがこのモジュール。
//!
//! ## 3 つの部品
//!
//! 1. **リース台帳** — `~/.zaivern/leases/<スコープキー>.json`。
//!    プロセスをまたいで見えることが要件 (判定するのは GUI ではなく、
//!    ベンダー CLI が起こす短命の `zai hook` プロセス)。
//!    確保は**アトミック**で、競合したら片方だけが勝つ (後勝ちにしない)。
//! 2. **強制** — `zai hook` が `PreToolUse` で書き込み系ツールを見たとき、
//!    他人が持っているパスなら **deny を返してツール呼び出しを止める**。
//! 3. **事前の重複検出** — N 人へ配る前に担当集合の重なりを出し、分割を促す。
//!
//! ## スコープは worktree ではなく「元のリポジトリ」
//!
//! ここが競合との差。`main_repo_root` は linked worktree の `.git` ファイル
//! (`gitdir: …/.git/worktrees/<名前>`) を辿って**元のリポジトリのルート**へ
//! 寄せる。そうしないと worktree ごとに台帳が分かれ、まさに調査が指摘した
//! 「worktree は意味的な衝突を 1 つも防がない」状態に戻ってしまう。
//!
//! ## 段 (CLAUDE.md 設計原則 4 の作法をそのまま適用)
//!
//! | 段 | 条件 | 効果 |
//! |---|---|---|
//! | 強制 | フックが設置済み | 書き込みが**実際に止まる** |
//! | 勧告 | 台帳はあるがフックが無い | UI が警告するだけ |
//! | 無効 | 台帳が無い | 何もしない (フックの追加コストは `stat` 1 回) |
//!
//! **「効いていると思わせて実は勧告」は無いより悪い。** so 段は画面に出す。
//!
//! ## 失敗の向き
//!
//! - **内部エラーは fail-open** (許可)。台帳が読めない・ロックが取れないのは
//!   こちらの都合で、それでユーザーのエージェントを止めるのは衝突より悪い。
//! - **本物の競合は fail-closed** (拒否)。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::i18n::{tr, trf};

// ═══════════════════════════════════════════════════════════════════════════
//  定数
// ═══════════════════════════════════════════════════════════════════════════

/// リースの既定寿命 (秒)。死んだエージェントにリポジトリを人質へ取らせない。
/// フック経由の自動確保は書き込みのたびに延長されるので、**30 分黙った
/// エージェントは所有権を失う**。
pub const DEFAULT_TTL_SECS: u64 = 30 * 60;

/// 期限切れ後、所有プロセスが生きている間だけ与える猶予 (秒)。
///
/// **上限が要る理由**: 生存確認は PID で行うが、PID は再利用される。
/// 猶予を無制限にすると、再利用された無関係な PID のせいでリースが
/// 永久に生き残り得る。CLAUDE.md の「終了済みセッションへ kill を撃たない」
/// と同じ懸念で、こちらは kill しない代わりに**寿命の上限で封じる**。
const RECLAIM_GRACE_SECS: u64 = 5 * 60;

/// 残りがこの割合を切ったら延長する (書き込みのたびに書き戻さない)。
const REFRESH_BELOW: f64 = 0.5;

/// 1 スコープに置ける最大リース数。壊れた書き手に台帳を膨らませない。
const MAX_LEASES: usize = 512;

/// ロック待ちの上限 (ミリ秒)。**エージェントの書き込みの臨界路**なので短い。
/// 取れなければ fail-open で許可する。
const LOCK_WAIT_MS: u64 = 200;

/// 置き去りロックを奪ってよくなるまでの時間 (ミリ秒)。
/// フックは短命なので、これを超えて握っているのはクラッシュの跡。
const LOCK_STALE_MS: u64 = 5_000;

/// 台帳のポーリング間隔の基準。実所要の 4 倍まで自動で空く
/// ([`crate::git::scan_interval`])。
const SCAN_BASE: Duration = Duration::from_millis(1_500);

/// 診断ログの上限 (バイト)。超えたら作り直す (無限に伸ばさない)。
const LOG_CAP: u64 = 64 * 1024;

/// 画面が狭いときにボタンをアイコンだけへ縮退させる境界 (pt)。
const COMPACT_WIDTH: f32 = 560.0;

// ═══════════════════════════════════════════════════════════════════════════
//  1. パスの正規化と glob (純粋関数 — ここが取り違えると全部が狂う)
// ═══════════════════════════════════════════════════════════════════════════

/// パス / パターンを台帳の正規形へ。
///
/// * 区切りは `/` へ寄せる (Windows の `\` をそのまま保存すると、
///   同じファイルが 2 つのキーで台帳に載る)
/// * 連続する区切りと `./` を潰す
/// * Windows は大文字小文字を区別しないファイルシステムが既定なので畳む。
///   **両方の側を実装する** — unix はそのまま (`Foo.rs` と `foo.rs` は別物)
pub fn normalize_path(raw: &str) -> String {
    let slashed = raw.replace('\\', "/");
    let mut out = String::with_capacity(slashed.len());
    for seg in slashed.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(seg);
    }
    if cfg!(windows) {
        out.to_lowercase()
    } else {
        out
    }
}

/// パターンを区切りごとの並びへ。
///
/// 末尾が `/` のものは**サブツリー指定**とみなして `**` を足す
/// (「auth モジュールを直して」と頼まれたエージェントは配下を丸ごと持つ)。
fn segments(pattern: &str) -> Vec<String> {
    let trailing_dir = pattern.ends_with('/') || pattern.ends_with('\\');
    let norm = normalize_path(pattern);
    let mut segs: Vec<String> = norm.split('/').map(str::to_string).collect();
    segs.retain(|s| !s.is_empty());
    if trailing_dir {
        segs.push("**".to_string());
    }
    segs
}

/// パターンが具体的なパスを覆うか。**フックの臨界路**なのでここは単純に保つ。
///
/// `path` 側は実在のパスなので `*` / `?` はワイルドカードとして扱わない
/// (ファイル名に `*` が入る環境では過剰一致し得る — Windows では不正文字、
/// unix でも実運用ではまず無い。既知の限界として受け入れる)。
pub fn covers(pattern: &str, path: &str) -> bool {
    seg_covers(&segments(pattern), &segments(path))
}

fn seg_covers(pat: &[String], path: &[String]) -> bool {
    let Some(head) = pat.first() else {
        return path.is_empty();
    };
    if head == "**" {
        // `**` は 0 個以上のセグメントに当たる。
        return (0..=path.len()).any(|k| seg_covers(&pat[1..], &path[k..]));
    }
    let Some(seg) = path.first() else {
        return false;
    };
    seg_one(head, seg) && seg_covers(&pat[1..], &path[1..])
}

/// 1 セグメント内の `*` / `?` 照合。
fn seg_one(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    // 素直な DP。セグメントは短い (せいぜい数十文字)。
    let mut reach = vec![false; t.len() + 1];
    reach[0] = true;
    for pc in p {
        let mut next = vec![false; t.len() + 1];
        for (j, &ok) in reach.iter().enumerate() {
            if !ok {
                continue;
            }
            match pc {
                '*' => {
                    // 0 文字以上: ここから後ろ全部へ届く
                    for n in next.iter_mut().skip(j) {
                        *n = true;
                    }
                }
                '?' => {
                    if j < t.len() {
                        next[j + 1] = true;
                    }
                }
                c => {
                    if j < t.len() && t[j] == c {
                        next[j + 1] = true;
                    }
                }
            }
        }
        reach = next;
    }
    reach[t.len()]
}

/// **2 つのパターンが同じパスに当たり得るか。** 事前の重複検出と、
/// 確保時の競合判定はどちらもこれ 1 本で決まる。
///
/// 難しいのは境界で、テーブルテストで固定してある:
/// `src/**` と `src/a.rs` は重なる / ファイルとその親ディレクトリは重なる /
/// `src/*.rs` と `src/sub/a.rs` は重ならない (`*` は `/` を越えない)。
pub fn overlaps(a: &str, b: &str) -> bool {
    seg_overlap(&segments(a), &segments(b))
}

fn seg_overlap(a: &[String], b: &[String]) -> bool {
    match (a.first(), b.first()) {
        (None, None) => true,
        // 片方が尽きたら、残りが全部 `**` (= 0 個に当たれる) のときだけ重なる。
        (None, Some(_)) => b.iter().all(|s| s == "**"),
        (Some(_), None) => a.iter().all(|s| s == "**"),
        (Some(x), Some(y)) => {
            if x == "**" {
                return seg_overlap(&a[1..], b) || seg_overlap(a, &b[1..]);
            }
            if y == "**" {
                return seg_overlap(a, &b[1..]) || seg_overlap(&a[1..], b);
            }
            seg_intersects(x, y) && seg_overlap(&a[1..], &b[1..])
        }
    }
}

/// 1 セグメントぶんのパターン同士が、共通の文字列に当たり得るか (DP)。
///
/// 素朴な再帰は `*` が並ぶと指数になるので、到達可能な `(i, j)` を
/// 幅優先で 1 回だけ塗る。
fn seg_intersects(x: &str, y: &str) -> bool {
    let a: Vec<char> = x.chars().collect();
    let b: Vec<char> = y.chars().collect();
    let (n, m) = (a.len(), b.len());
    let idx = |i: usize, j: usize| i * (m + 1) + j;
    let mut seen = vec![false; (n + 1) * (m + 1)];
    let mut stack = vec![(0usize, 0usize)];
    seen[0] = true;
    while let Some((i, j)) = stack.pop() {
        if i == n && j == m {
            return true;
        }
        let mut push = |i: usize, j: usize, stack: &mut Vec<(usize, usize)>| {
            if !seen[idx(i, j)] {
                seen[idx(i, j)] = true;
                stack.push((i, j));
            }
        };
        match (a.get(i), b.get(j)) {
            (Some('*'), _) => {
                // `*` は 0 文字で終わる / 相手の 1 文字を飲む
                push(i + 1, j, &mut stack);
                if j < m {
                    push(i, j + 1, &mut stack);
                }
            }
            (_, Some('*')) => {
                push(i, j + 1, &mut stack);
                if i < n {
                    push(i + 1, j, &mut stack);
                }
            }
            (Some(&ca), Some(&cb)) => {
                if ca == '?' || cb == '?' || ca == cb {
                    push(i + 1, j + 1, &mut stack);
                }
            }
            // 片方だけ尽きた: 残りが全部 `*` なら空文字に当たれる
            (None, Some(_)) => {
                if b[j..].iter().all(|c| *c == '*') {
                    return true;
                }
            }
            (Some(_), None) => {
                if a[i..].iter().all(|c| *c == '*') {
                    return true;
                }
            }
            (None, None) => return true,
        }
    }
    false
}

// ═══════════════════════════════════════════════════════════════════════════
//  2. スコープ — 「どのリポジトリの話か」
// ═══════════════════════════════════════════════════════════════════════════

/// linked worktree の `.git` ファイルの中身から、**元のリポジトリのルート**を出す。
///
/// 中身は `gitdir: <元のリポジトリ>/.git/worktrees/<名前>` の 1 行。
/// `worktrees/<名前>` を 2 つ落とすと `.git` に戻り、その親が元のルート。
/// 形が違えば `None` (推測しない)。
pub fn main_repo_root_from_pointer(text: &str) -> Option<PathBuf> {
    let line = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("gitdir:"))?;
    let gitdir = PathBuf::from(line.trim());
    // …/.git/worktrees/<名前> → …/.git → …
    let git = gitdir.parent()?.parent()?;
    if git.file_name().and_then(|s| s.to_str()) != Some(".git") {
        return None;
    }
    git.parent().map(Path::to_path_buf)
}

/// 台帳のキーになるルートと、パスを相対化する作業ツリーのルート。
///
/// **この 2 つは linked worktree では別物**で、そこを取り違えると機能が
/// 丸ごと無言で効かなくなる (実際に e2e で踏んだ: worktree のファイルは
/// 元のリポジトリの配下に**無い**ので、元リポジトリ基準の相対化が必ず失敗し、
/// 全部「スコープ外」として素通りしていた)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roots {
    /// 台帳のキー = **元のリポジトリのルート**。worktree 群が 1 つの台帳を共有する。
    pub key: PathBuf,
    /// パスの相対化に使う = **いまいる作業ツリーのルート**。
    pub tree: PathBuf,
}

/// 与えられた場所から [`Roots`] を出す。
///
/// 1. 上へ辿って最初の `.git` を探す → そこが `tree`
/// 2. `.git` がファイルなら linked worktree → `key` は元のリポジトリへ寄せる
///    (**ここを寄せないと worktree ごとに台帳が割れて、この機能の意味が消える**)
/// 3. `.git` が見つからなければ、その場所自身 (git 管理でないフォルダでも動く)
///
/// 返り値は必ず同じ正規形にする。片方だけ canonicalize すると、macOS の
/// `/var` → `/private/var` のようなシンボリックリンクで同じリポジトリが
/// 2 つのキーへ割れる (これもテストで踏んだ)。
pub fn roots_of(start: &Path) -> Roots {
    let (key, tree) = roots_raw(start);
    Roots {
        key: key.canonicalize().unwrap_or(key),
        tree: tree.canonicalize().unwrap_or(tree),
    }
}

fn roots_raw(start: &Path) -> (PathBuf, PathBuf) {
    let base = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    for dir in base.ancestors() {
        let dot = dir.join(".git");
        if dot.is_dir() {
            return (dir.to_path_buf(), dir.to_path_buf());
        }
        if dot.is_file() {
            let main = std::fs::read_to_string(&dot)
                .ok()
                .and_then(|t| main_repo_root_from_pointer(&t))
                .unwrap_or_else(|| dir.to_path_buf());
            return (main, dir.to_path_buf());
        }
    }
    (base.clone(), base)
}

/// **まだ存在しないパスでも**実在する祖先まで解決する canonicalize。
///
/// 素の [`Path::canonicalize`] は存在しないパスで失敗する。そこで諦めると
/// **`Write` による新規ファイル作成が丸ごと素通りする** — 台帳側は
/// canonicalize 済み (macOS なら `/private/var/…`) なのに、対象だけ
/// 生のパス (`/var/…`) のままになり、前方一致が必ず外れるため。
fn canonical_best_effort(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    while let Some(name) = cur.file_name().map(|s| s.to_os_string()) {
        let Some(parent) = cur.parent().map(Path::to_path_buf) else {
            break;
        };
        rest.push(name);
        if let Ok(c) = parent.canonicalize() {
            let mut out = c;
            for r in rest.iter().rev() {
                out.push(r);
            }
            return out;
        }
        if parent.as_os_str().is_empty() {
            break;
        }
        cur = parent;
    }
    p.to_path_buf()
}

/// ルートからの相対パス (正規形)。ルートの外なら `None` = **関知しない**。
pub fn rel_within(root: &Path, target: &Path) -> Option<String> {
    let t = canonical_best_effort(target);
    let s = canonical_best_effort(root);
    let rel = t.strip_prefix(&s).ok()?;
    let norm = normalize_path(&rel.to_string_lossy());
    if norm.is_empty() {
        None
    } else {
        Some(norm)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  3. 台帳の型
// ═══════════════════════════════════════════════════════════════════════════

/// 持ち主。**ベンダーのセッション ID が第一の身元**で、無ければ作業フォルダ。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    /// 画面と拒否理由に出す名前 (エージェント名 / セッション名)。
    #[serde(default)]
    pub agent: String,
    /// ベンダーが振ったセッション ID。空なら `cwd` で照合する。
    #[serde(default)]
    pub session: String,
    /// 作業フォルダ (正規形)。
    #[serde(default)]
    pub cwd: String,
    /// 生存確認に使う PID。0 = 確認手段なし (TTL だけで回収)。
    #[serde(default)]
    pub pid: u32,
}

impl Holder {
    /// 画面に出す 1 行の名前。
    pub fn display(&self) -> String {
        if self.agent.is_empty() {
            tr("(名前なし)")
        } else if self.session.is_empty() {
            self.agent.clone()
        } else {
            let short: String = self.session.chars().take(8).collect();
            format!("{} #{short}", self.agent)
        }
    }

    /// 同じ持ち主か。**セッション ID が両方にあるならそれだけで決める** —
    /// 同じフォルダで 2 セッション走っていても取り違えない。
    pub fn same(&self, other: &Holder) -> bool {
        if !self.session.is_empty() && !other.session.is_empty() {
            return self.session == other.session;
        }
        !self.cwd.is_empty() && self.cwd == other.cwd && self.agent == other.agent
    }
}

/// リース 1 件。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub holder: Holder,
    /// 所有するパターン (スコープからの相対、`/` 区切り、glob 可)。
    #[serde(default)]
    pub patterns: Vec<String>,
    /// 確保した時刻 (UNIX 秒)。
    #[serde(default)]
    pub acquired_at: u64,
    /// 期限 (UNIX 秒)。
    #[serde(default)]
    pub expires_at: u64,
    /// 何のための確保か (拒否理由に出す)。
    #[serde(default)]
    pub note: String,
}

impl Lease {
    /// このリースが具体的なパスを覆うか。
    pub fn covers_path(&self, rel: &str) -> bool {
        self.patterns.iter().any(|p| covers(p, rel))
    }

    /// まだ効いているか。
    ///
    /// 期限内なら当然有効。期限切れでも**持ち主のプロセスが生きている間は
    /// [`RECLAIM_GRACE_SECS`] だけ猶予する** (エージェントが戻ってきたときに
    /// 所有を奪い返されないため)。猶予に上限があるのが肝で、PID 再利用で
    /// 「生きている」と誤判定しても永久には残らない。
    pub fn active(&self, now: u64, alive: &dyn Fn(u32) -> bool) -> bool {
        if now < self.expires_at {
            return true;
        }
        if self.holder.pid == 0 {
            return false;
        }
        now <= self.expires_at.saturating_add(RECLAIM_GRACE_SECS) && alive(self.holder.pid)
    }
}

/// 1 スコープぶんの台帳。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub leases: Vec<Lease>,
}

// ═══════════════════════════════════════════════════════════════════════════
//  4. 純粋な判断 (I/O を一切しない — テーブルテストで固定する部分)
// ═══════════════════════════════════════════════════════════════════════════

/// 確保の結果。**競合したら fail-closed** で、後勝ちにはしない。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Claim {
    /// 取れた (件数は新たに足したパターン数)。
    Granted(usize),
    /// 他人が持っている。
    Refused {
        owner: String,
        pattern: String,
        until: u64,
    },
}

/// 失効したリースを落とす。
pub fn prune(store: &mut Store, now: u64, alive: &dyn Fn(u32) -> bool) {
    store.leases.retain(|l| l.active(now, alive));
}

/// パターン群を確保する。**1 つでも他人と重なれば 1 つも取らない** (全か無か)。
///
/// 全か無かにするのは、部分的に取れた状態がいちばん危ないため —
/// エージェントは「取れた」と思って作業を始め、取れなかったパスで衝突する。
pub fn try_claim(
    store: &mut Store,
    holder: &Holder,
    patterns: &[String],
    now: u64,
    ttl: u64,
    alive: &dyn Fn(u32) -> bool,
) -> Claim {
    prune(store, now, alive);
    let wanted: Vec<String> = patterns
        .iter()
        .map(|p| normalize_path(p))
        .filter(|p| !p.is_empty())
        .collect();
    for l in store.leases.iter().filter(|l| !l.holder.same(holder)) {
        for w in &wanted {
            if let Some(hit) = l.patterns.iter().find(|p| overlaps(p, w)) {
                return Claim::Refused {
                    owner: l.holder.display(),
                    pattern: hit.clone(),
                    until: l.expires_at,
                };
            }
        }
    }
    let expires = now.saturating_add(ttl);
    if let Some(mine) = store.leases.iter_mut().find(|l| l.holder.same(holder)) {
        let mut added = 0;
        for w in wanted {
            if !mine.patterns.contains(&w) {
                mine.patterns.push(w);
                added += 1;
            }
        }
        mine.expires_at = mine.expires_at.max(expires);
        // 持ち主の表示名と PID は最後に見たものへ更新する (名前が付いた等)。
        if !holder.agent.is_empty() {
            mine.holder.agent = holder.agent.clone();
        }
        if holder.pid != 0 {
            mine.holder.pid = holder.pid;
        }
        return Claim::Granted(added);
    }
    if store.leases.len() >= MAX_LEASES {
        // 壊れた書き手に台帳を膨らませない。fail-open (許可) 側へ倒す。
        return Claim::Granted(0);
    }
    let n = wanted.len();
    store.leases.push(Lease {
        holder: holder.clone(),
        patterns: wanted,
        acquired_at: now,
        expires_at: expires,
        note: String::new(),
    });
    Claim::Granted(n)
}

/// 持ち主のリースを手放す。返り値は消した件数。
pub fn release(store: &mut Store, holder: &Holder) -> usize {
    let before = store.leases.len();
    store.leases.retain(|l| !l.holder.same(holder));
    before - store.leases.len()
}

/// フックの答え。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// 通す (自分のもの / 誰も持っていない)。
    Allow,
    /// 止める。文面は**そのままエージェントとユーザーに見せる**。
    Deny(String),
}

/// **1 パスに対する判定。** ここがこの機能の心臓で、I/O を持たない。
pub fn decide(
    store: &Store,
    holder: &Holder,
    rel: &str,
    now: u64,
    alive: &dyn Fn(u32) -> bool,
) -> Verdict {
    for l in &store.leases {
        if !l.active(now, alive) || !l.covers_path(rel) {
            continue;
        }
        if l.holder.same(holder) {
            return Verdict::Allow;
        }
        return Verdict::Deny(deny_reason(rel, l, now));
    }
    Verdict::Allow
}

/// 拒否の文面。**「拒否されました」だけでは、ユーザーは機能を切るだけ。**
/// 誰が・いつから持っていて・どうすればよいかを必ず出す。
fn deny_reason(rel: &str, l: &Lease, now: u64) -> String {
    let since = crate::instances::humanize_uptime(now.saturating_sub(l.acquired_at));
    let left = crate::instances::humanize_uptime(l.expires_at.saturating_sub(now));
    let note = if l.note.is_empty() {
        String::new()
    } else {
        trf("\n目的: {note}", &[("note", l.note.clone())])
    };
    trf(
        "「{path}」は {owner} が確保しています ({since}前から / 期限まであと {left})。{note}\n\
         同じファイルを 2 人が同時に編集すると、衝突はマージのときまで見えません。\n\
         対処: (1) {owner} の完了を待つ (2) 担当を分ける — 別のファイル / 別のディレクトリを受け持つ \
         (3) 引き継ぐなら Zaivern Code のコマンドパレットで「ファイル所有の一覧」を開き、該当のリースを解放する",
        &[
            ("path", rel.to_string()),
            ("owner", l.holder.display()),
            ("since", since),
            ("left", left),
            ("note", note),
        ],
    )
}

// ═══════════════════════════════════════════════════════════════════════════
//  5. 台帳の入出力 (アトミック / fail-open)
// ═══════════════════════════════════════════════════════════════════════════

/// 台帳の置き場所 (`~/.zaivern/leases/`)。
pub fn store_dir() -> PathBuf {
    crate::config::zaivern_dir().join("leases")
}

/// スコープに対応する台帳ファイル。キーは `history::workspace_key` と共通なので、
/// GUI と `zai hook` が**必ず同じファイルへ行き着く**。
pub fn store_path_in(dir: &Path, scope: &Path) -> PathBuf {
    dir.join(format!("{}.json", crate::history::workspace_key(scope)))
}

/// このスコープで機能が有効か。
///
/// **有効化はファイルの存在**で表す。無効なら `zai hook` は `stat` 1 回で
/// 抜けるので、使っていないユーザーが払うコストが実質ゼロになる
/// (設計原則 3: アイドル時のコストはゼロ)。
pub fn enabled(store: &Path) -> bool {
    store.exists()
}

/// このスコープで有効にする (空の台帳を置く)。既にあれば何もしない。
pub fn enable(store: &Path) -> Result<(), String> {
    if store.exists() {
        return Ok(());
    }
    write_store(store, &Store::default())
}

/// 台帳を読む。無ければ空、**壊れていれば `Err`** (握り潰さない)。
pub fn read_store(store: &Path) -> Result<Store, String> {
    let raw = match std::fs::read_to_string(store) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(e) => return Err(format!("台帳を読めません: {e}")),
    };
    if raw.trim().is_empty() {
        return Ok(Store::default());
    }
    serde_json::from_str(&raw).map_err(|e| format!("台帳が壊れています: {e}"))
}

/// 台帳を書く。**tmp → rename** なので、読み手が書きかけを見ることはない。
fn write_store(store: &Path, s: &Store) -> Result<(), String> {
    if let Some(dir) = store.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("台帳フォルダを作れません: {e}"))?;
    }
    let json = serde_json::to_string_pretty(s).map_err(|e| format!("JSON 化に失敗: {e}"))?;
    let tmp = store.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, json).map_err(|e| format!("台帳を書けません: {e}"))?;
    // rename は同一ディレクトリ内なら unix / Windows とも置換が保証される。
    std::fs::rename(&tmp, store).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("台帳を差し替えられません: {e}")
    })
}

/// 排他ロック。`create_new` は OS の `O_EXCL` / `CREATE_NEW` に落ちるので、
/// **同時に来た 2 プロセスのうち 1 つだけが成功する** (後勝ちにならない)。
struct LockGuard(PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn acquire_lock(store: &Path) -> Result<LockGuard, String> {
    let path = store.with_extension("lock");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("台帳フォルダを作れません: {e}"))?;
    }
    let deadline = Instant::now() + Duration::from_millis(LOCK_WAIT_MS);
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(LockGuard(path)),
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(format!("ロックを作れません: {e}"))
            }
            Err(_) => {}
        }
        // クラッシュで置き去りになったロックは奪う (でないと永久に詰まる)。
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d.as_millis() as u64 > LOCK_STALE_MS);
        if stale {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if Instant::now() >= deadline {
            return Err(tr("ロックを取れませんでした (先客が握っています)"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// ロックを取って読み → 変更 → 書き戻す。**確保の唯一の入口**。
pub fn with_store<T>(store: &Path, f: impl FnOnce(&mut Store) -> T) -> Result<T, String> {
    let _lock = acquire_lock(store)?;
    let mut s = read_store(store)?;
    let out = f(&mut s);
    write_store(store, &s)?;
    Ok(out)
}

/// 診断ログ (`~/.zaivern/leases/gate.log`)。**拒否と内部エラーだけ**書く。
/// 許可のたびに書くとエージェントの臨界路で I/O が増える。
fn log_line(dir: &Path, line: &str) {
    use std::io::Write;
    let path = dir.join("gate.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_CAP) {
        let _ = std::fs::remove_file(&path);
    }
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{} {line}", now_secs());
    }
}

/// 現在時刻 (UNIX 秒)。時計が epoch 以前でも落とさない。
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// PID の生存確認 (既定の実装)。テストは偽の関数を渡す。
fn pid_alive(pid: u32) -> bool {
    crate::instances::pid_alive(pid)
}

// ═══════════════════════════════════════════════════════════════════════════
//  6. フック経路 — 強制はここでしか起きない
// ═══════════════════════════════════════════════════════════════════════════

/// ベンダーへ返す答え。`stdout` が空なら「判断しない」= 通常の許可フローへ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookAnswer {
    pub stdout: String,
    pub stderr: String,
    pub exit: i32,
}

/// 拒否を Claude Code の `PreToolUse` 出力スキーマへ (実ドキュメントで確認済み)。
///
/// ```text
/// {"hookSpecificOutput":{"hookEventName":"PreToolUse",
///  "permissionDecision":"deny","permissionDecisionReason":"…"}}
/// ```
/// 終了コードは **0**。ドキュメント曰く「JSON output is only processed on exit 0」で、
/// exit 2 にすると stdout は無視され stderr がエラーとして流れる。
/// ここでは正規の permission decision を使う (エラーではなく判断なので)。
///
/// **許可のときは何も出さない。** `"allow"` を返すとユーザー自身の許可設定を
/// 飛び越えてしまう — こちらが与えたいのは「止める権限」だけで、
/// 「他人の確認を省く権限」ではない。
pub fn deny_answer(reason: &str) -> HookAnswer {
    let json = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    });
    HookAnswer {
        stdout: json.to_string(),
        // stderr にも出す: 終了コードだけを見るベンダーや、ログを追う人向け。
        stderr: reason.to_string(),
        exit: 0,
    }
}

/// 判断しない (通常の許可フローへ戻す)。
pub fn pass_answer() -> HookAnswer {
    HookAnswer {
        stdout: String::new(),
        stderr: String::new(),
        exit: 0,
    }
}

/// ペイロードから書き込み先のパスを取り出す **純関数**。
///
/// キーはエージェント固有なので [`crate::agents::HOOK_TARGETS`] の
/// `write_path_keys` から渡す (ここにリテラルを置かない)。
pub fn target_path(payload: &serde_json::Value, keys: &[&str]) -> String {
    let input = payload.get("tool_input").unwrap_or(payload);
    for k in keys {
        if let Some(s) = input.get(*k).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    // MultiEdit 系: edits[] の中にパスが入る形にも対応する。
    if let Some(arr) = input.get("edits").and_then(|v| v.as_array()) {
        for e in arr {
            for k in keys {
                if let Some(s) = e.get(*k).and_then(|v| v.as_str()) {
                    if !s.is_empty() {
                        return s.to_string();
                    }
                }
            }
        }
    }
    String::new()
}

/// `zai hook` から呼ぶ**強制の本体**。GUI が動いていなくても効く。
///
/// 3 つの制約 (どれも load-bearing):
/// * **速いこと** — 書き込みのたびに通る。無効なら `stat` 1 回で戻る。
///   リポジトリを走査しない。
/// * **内部エラーは fail-open** — 台帳が読めない / ロックが取れないで
///   ユーザーのエージェントを止めない。
/// * **本物の競合は fail-closed**。
pub fn gate(agent: &str, event: &str, payload: &str) -> HookAnswer {
    let v: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return pass_answer(), // 読めない = こちらの都合。通す
    };
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let event = if event.is_empty() {
        s("hook_event_name")
    } else {
        event.to_string()
    };
    if event != "PreToolUse" {
        return pass_answer();
    }
    let Some(target) = crate::agents::hook_target(agent) else {
        return pass_answer(); // カタログに無いエージェント = 形が判らない
    };
    // 「書き込み系ツールか」もカタログから引く (ここにツール名を書かない)。
    let tool = s("tool_name");
    if crate::agents::hook_tool_state(agent, &tool)
        != Some(crate::supervisor::protocol::ProtoState::Editing)
    {
        return pass_answer();
    }
    let raw = target_path(&v, target.write_path_keys);
    if raw.is_empty() {
        return pass_answer();
    }
    let cwd = s("cwd");
    let cwd = if cwd.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(cwd)
    };
    let roots = roots_of(&cwd);
    let dir = store_dir();
    let store = store_path_in(&dir, &roots.key);
    // ここが「使っていない人が払う全コスト」= stat 1 回。
    if !enabled(&store) {
        return pass_answer();
    }
    // 相対パスへ。**相対化は作業ツリー基準**で行う (worktree のファイルは
    // 元のリポジトリの配下に無いので、key 基準にすると必ず外れる)。
    // ツリーの外 (別リポジトリ・システムのファイル) は関知しない。
    let abs = if Path::new(&raw).is_absolute() {
        PathBuf::from(&raw)
    } else {
        cwd.join(&raw)
    };
    let Some(rel) = rel_within(&roots.tree, &abs) else {
        return pass_answer();
    };
    let holder = Holder {
        agent: agent.to_string(),
        session: s("session_id"),
        cwd: normalize_path(&cwd.to_string_lossy()),
        pid: 0, // フックは短命プロセス。生存確認には使えないので TTL に委ねる
    };
    let now = now_secs();
    let alive: &dyn Fn(u32) -> bool = &pid_alive;

    // 1 回のロックで「判定 → 自動確保 / 延長」まで済ませる (往復を増やさない)。
    let outcome = with_store(&store, |st| {
        prune(st, now, alive);
        match decide(st, &holder, &rel, now, alive) {
            Verdict::Deny(reason) => Verdict::Deny(reason),
            Verdict::Allow => {
                // **誰も持っていないなら、書いた本人のものにする。**
                // これがあるから、ユーザーが 1 件も設定しなくても
                // 2 人目が同じファイルへ来た瞬間に止まる。
                let refresh = st
                    .leases
                    .iter()
                    .find(|l| l.holder.same(&holder))
                    .is_none_or(|l| {
                        let left = l.expires_at.saturating_sub(now) as f64;
                        left < DEFAULT_TTL_SECS as f64 * REFRESH_BELOW
                    });
                if refresh || !st.leases.iter().any(|l| l.covers_path(&rel)) {
                    let _ = try_claim(st, &holder, &[rel.clone()], now, DEFAULT_TTL_SECS, alive);
                }
                Verdict::Allow
            }
        }
    });
    match outcome {
        Ok(Verdict::Deny(reason)) => {
            log_line(&dir, &format!("deny {} {rel}", holder.display()));
            deny_answer(&reason)
        }
        Ok(Verdict::Allow) => pass_answer(),
        Err(e) => {
            // fail-open。**自分のバグでユーザーの作業を止めない。**
            log_line(&dir, &format!("fail-open {rel}: {e}"));
            pass_answer()
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  7. 事前の重複検出 — いちばん安い勝ち
// ═══════════════════════════════════════════════════════════════════════════

/// 「この担当にこのファイル群」の 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    pub agent: String,
    pub patterns: Vec<String>,
}

/// 重なり 1 件 (どの 2 人が、どのパターンで)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overlap {
    pub a: usize,
    pub b: usize,
    pub pattern_a: String,
    pub pattern_b: String,
}

/// `名前: パターン, パターン` の行を割り当てへ。空行と `#` 始まりは無視。
pub fn parse_assignments(text: &str) -> Vec<Assignment> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (agent, rest) = match line.split_once(':') {
            Some((a, r)) => (a.trim().to_string(), r),
            None => (format!("#{}", out.len() + 1), line),
        };
        let patterns: Vec<String> = rest
            .split([',', ' ', '\t'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(normalize_path)
            .collect();
        if !patterns.is_empty() {
            out.push(Assignment { agent, patterns });
        }
    }
    out
}

/// **配る前に**重なりを全部出す。O(担当数² × パターン数²) だがどれも小さい。
pub fn plan_overlaps(list: &[Assignment]) -> Vec<Overlap> {
    let mut out = Vec::new();
    for i in 0..list.len() {
        for j in (i + 1)..list.len() {
            for pa in &list[i].patterns {
                for pb in &list[j].patterns {
                    if overlaps(pa, pb) {
                        out.push(Overlap {
                            a: i,
                            b: j,
                            pattern_a: pa.clone(),
                            pattern_b: pb.clone(),
                        });
                    }
                }
            }
        }
    }
    out
}

/// 警告だけでは足りない — **使える手**を出す。
///
/// 重なったパターンを後の担当から外した「互いに素な」割り当てを返す。
/// 外した分は誰も持たなくなるので、直列にやるべき部分として一覧に出す。
pub fn split_plan(list: &[Assignment]) -> (Vec<Assignment>, Vec<String>) {
    let mut taken: Vec<String> = Vec::new();
    let mut serial: Vec<String> = Vec::new();
    let mut out: Vec<Assignment> = Vec::new();
    for a in list {
        let mut keep = Vec::new();
        for p in &a.patterns {
            if taken.iter().any(|t| overlaps(t, p)) {
                if !serial.contains(p) {
                    serial.push(p.clone());
                }
            } else {
                taken.push(p.clone());
                keep.push(p.clone());
            }
        }
        out.push(Assignment {
            agent: a.agent.clone(),
            patterns: keep,
        });
    }
    (out, serial)
}

// ═══════════════════════════════════════════════════════════════════════════
//  8. 段 (どこまで効いているかを正直に出す)
// ═══════════════════════════════════════════════════════════════════════════

/// 効力の段。**「効いていると思わせて実は勧告」は無いより悪い。**
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tier {
    /// フックが設置済み — 書き込みが実際に止まる
    Enforced,
    /// 台帳はあるがフックが無い — 画面が警告するだけ
    Advisory,
    /// 無効
    #[default]
    Off,
}

impl Tier {
    /// UI に出す短い名前 (tr のキーになる日本語原文)。
    pub fn label(self) -> &'static str {
        match self {
            Tier::Enforced => "強制",
            Tier::Advisory => "勧告",
            Tier::Off => "無効",
        }
    }

    /// 何が起きるかの 1 行。
    pub fn detail(self) -> &'static str {
        match self {
            Tier::Enforced => "他人が確保しているファイルへの書き込みは、実際にブロックされます",
            Tier::Advisory => {
                "所有は記録しますが、ブロックはしません (フックを設置すると強制になります)"
            }
            Tier::Off => "このワークスペースでは何もしていません",
        }
    }

    /// 段に対応する色。
    ///
    /// `theme::Theme::ok` は egui の `Visuals` へ写されていない
    /// (`theme::apply` が移すのは panel / accent / border など) ため、
    /// 「成功」だけは明暗 2 通りをここで持つ。値は
    /// [`tests::段の色はどのテーマでも読める`] が全 11 テーマの背景に対して
    /// コントラスト比を検算している (WCAG AA 大文字 3.0 以上)。
    pub fn color(self, v: &egui::Visuals) -> egui::Color32 {
        match self {
            Tier::Enforced => {
                if v.dark_mode {
                    egui::Color32::from_rgb(0x7e, 0xc6, 0x99)
                } else {
                    egui::Color32::from_rgb(0x11, 0x6b, 0x3a)
                }
            }
            Tier::Advisory => v.warn_fg_color,
            Tier::Off => v.weak_text_color(),
        }
    }
}

/// 段の決め方 (純粋)。
pub fn tier(store_exists: bool, hook_installed: bool) -> Tier {
    match (store_exists, hook_installed) {
        (true, true) => Tier::Enforced,
        (true, false) => Tier::Advisory,
        (false, _) => Tier::Off,
    }
}

/// いまの段を実際に調べる (I/O)。**UI スレッドから直接呼ばない**。
///
/// 2 つのルートを取り違えないこと: 台帳は `key` (元のリポジトリ)、
/// フックの設定ファイル (`.claude/settings.json`) は `tree` (いまの作業ツリー)。
/// 片方で両方を引くと「有効にした直後に無効と出る」(実際に e2e で出した)。
pub fn current_tier(roots: &Roots) -> Tier {
    let store = store_path_in(&store_dir(), &roots.key);
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("zai"));
    // 1 つでも設置済みなら「強制」。カタログを回すのでリテラルは持たない。
    let installed = crate::agents::HOOK_TARGETS.iter().any(|t| {
        crate::supervisor::hooks::plan_for(t.bin, &roots.tree, &exe)
            .map(|p| crate::supervisor::hooks::status(&p))
            == Some(crate::supervisor::hooks::HookStatus::Installed)
    });
    tier(enabled(&store), installed)
}

// ═══════════════════════════════════════════════════════════════════════════
//  9. レイアウト (純粋関数 — 極端な寸法でテーブルテストする)
// ═══════════════════════════════════════════════════════════════════════════

/// 1 行ぶんの矩形。**どの幅でも見切れないこと**を関数で保証する。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowLayout {
    pub owner: egui::Rect,
    pub patterns: egui::Rect,
    pub left: egui::Rect,
    pub actions: egui::Rect,
}

/// 幅が狭いときはボタンをアイコンだけへ縮退させる。
pub fn is_compact(width: f32) -> bool {
    width < COMPACT_WIDTH
}

/// 行のレイアウト。可用領域・最長の持ち主名から列幅を決める。
///
/// 決め方:
/// * 操作列は固定 (狭いときはアイコン 1 個ぶん)
/// * 残り期限は固定
/// * 持ち主は「最長の名前」と「可用幅の 30%」の小さい方、下限あり
/// * パターン列が残り全部を取る (**必ず 0 以上**に切り詰める)
pub fn row_layout(avail: egui::Rect, longest_owner: f32) -> RowLayout {
    const GAP: f32 = 8.0;
    let w = avail.width();
    let actions = if is_compact(w) { 30.0 } else { 76.0 };
    let left = if is_compact(w) { 52.0 } else { 88.0 };
    // 下限 40pt。可用幅が極端に狭いときでも負にしない。
    let owner = longest_owner.clamp(40.0, (w * 0.30).max(40.0));
    // 固定列 + 隙間を引いた残り。負にならないよう 0 で止める。
    let fixed = owner + left + actions + GAP * 3.0;
    let patterns = (w - fixed).max(0.0);
    // 残りが足りないときは持ち主列から削る (パターン列を最低 40pt 確保)。
    let (owner, patterns) = if patterns < 40.0 {
        let want = 40.0f32.min((w - left - actions - GAP * 3.0).max(0.0));
        let o = (w - left - actions - GAP * 3.0 - want).max(0.0);
        (o, want)
    } else {
        (owner, patterns)
    };
    let y = avail.y_range();
    let mut x = avail.left();
    let mut col = |width: f32| {
        let r = egui::Rect::from_x_y_ranges(x..=(x + width), y);
        x += width + GAP;
        r
    };
    RowLayout {
        owner: col(owner),
        patterns: col(patterns),
        left: col(left),
        actions: col(actions),
    }
}

/// 空状態のカード。**利用可能領域の中央**に 1 枚 (下や上に取り残さない)。
pub fn empty_card(avail: egui::Rect) -> egui::Rect {
    let w = (avail.width() * 0.72).clamp(0.0, 420.0).min(avail.width());
    let h = 132.0f32.min(avail.height());
    egui::Rect::from_center_size(avail.center(), egui::vec2(w, h))
}

// ═══════════════════════════════════════════════════════════════════════════
//  10. UI — パレットから開くパネル
// ═══════════════════════════════════════════════════════════════════════════

/// パレットへの登録。**共有ファイルを 1 バイトも触らずに機能が繋がる**入口
/// (`src/features/lease.rs` が `pub use` するだけで build.rs が拾う)。
///
/// 打鍵は割り当てていない — `keybinds::BindAction` は固定長配列 + 件数検査を
/// 持つ最も硬い共有面で、機能ブランチ側から増やすと直列マージが必ず衝突する。
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "lease",
    entries: &[crate::feature::Entry {
        icon: "🔐",
        label: "ファイル所有の一覧 — 並列エージェントの衝突を防ぐ",
        id: "lease.list",
    }],
    dispatch: |_app, _ctx, id| match id {
        "lease.list" => {
            open_panel();
            true
        }
        _ => false,
    },
    // パネルはウィンドウとして自分で描く (`app.rs` のビュー列挙に触らない)。
    draw: Some(draw),
};

/// 台帳の非同期読み取り 1 回ぶん。
struct Snapshot {
    store: Result<Store, String>,
    tier: Tier,
    cost: Duration,
}

/// パネルの状態。**ウィンドウより長生きさせる** (設計原則 1) ため、
/// `ZaivernApp` のフィールドではなくモジュール側に置く。
/// こうすると `app.rs` を 1 バイトも触らずに機能が繋がる。
#[derive(Default)]
struct PanelState {
    open: bool,
    roots: Roots,
    store: Store,
    tier: Tier,
    error: String,
    toast: String,
    /// 走っている読み取り。UI スレッドは**絶対に待たない**。
    pending: Option<Receiver<Snapshot>>,
    last_scan: Option<Instant>,
    last_cost: Option<Duration>,
    /// 事前チェックの入力欄。
    plan_text: String,
    /// 自分で確保するときのパターン。
    claim_text: String,
}

fn state() -> &'static Mutex<PanelState> {
    static S: OnceLock<Mutex<PanelState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(PanelState::default()))
}

/// GUI が開いているワークスペースのルート。
///
/// `app.rs` へ触らずに済ませるため、**自分自身のインスタンス登録**
/// (`~/.zaivern/instances/<pid>.json`) から引く。登録が無い / 壊れている
/// ときはカレントディレクトリへ落ちる (fail-soft)。
fn gui_workspace_root() -> PathBuf {
    let me = std::process::id();
    let found = crate::instances::scan_and_prune(&crate::instances::instances_dir())
        .into_iter()
        .find(|e| e.pid == me)
        .and_then(|e| e.workspace_roots.first().map(PathBuf::from));
    found.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// パレットの項目から呼ぶ入口。
pub fn open_panel() {
    let roots = roots_of(&gui_workspace_root());
    if let Ok(mut st) = state().lock() {
        st.open = true;
        st.roots = roots;
        st.last_scan = None; // 開いた回だけ必ず取り直す
        st.toast.clear();
    }
}

/// 台帳の読み取りを**裏のスレッド**へ出す。UI は手元の値を描き続ける。
///
/// git の教訓と同じで、UI スレッドで同期 I/O を撃つと最悪のときにフレームが
/// 止まる (実測: 同期 `git branch --show-current` が 6023ms / 最悪フレーム 4376ms)。
/// 台帳は小さいが、ロック待ち最大 200ms が乗り得るので裏へ出す。
fn spawn_scan(roots: Roots) -> Receiver<Snapshot> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let store = read_store(&store_path_in(&store_dir(), &roots.key));
        let tier = current_tier(&roots);
        let _ = tx.send(Snapshot {
            store,
            tier,
            cost: t0.elapsed(),
        });
    });
    rx
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
    let mut action = PanelAction::None;
    egui::Window::new(tr("🔐 ファイル所有の一覧"))
        .collapsible(false)
        .resizable(true)
        .default_width(660.0)
        .default_height(480.0)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            action = body(ui, &mut st);
        });
    if !open {
        st.open = false;
    }
    apply(&mut st, action);
}

/// パネルが要求した副作用 (描画の中では I/O をしない)。
enum PanelAction {
    None,
    Enable,
    Release(usize),
    Claim,
    Refresh,
}

fn apply(st: &mut PanelState, action: PanelAction) {
    let store_path = store_path_in(&store_dir(), &st.roots.key);
    match action {
        PanelAction::None => {}
        PanelAction::Refresh => st.last_scan = None,
        PanelAction::Enable => {
            st.toast = match enable(&store_path) {
                Ok(()) => tr("このワークスペースでファイル所有リースを有効にしました"),
                Err(e) => e,
            };
            st.last_scan = None;
        }
        PanelAction::Release(i) => {
            let Some(holder) = st.store.leases.get(i).map(|l| l.holder.clone()) else {
                return;
            };
            let n = with_store(&store_path, |s| release(s, &holder));
            st.toast = match n {
                Ok(n) => trf("{n} 件のリースを解放しました", &[("n", n.to_string())]),
                Err(e) => e,
            };
            st.last_scan = None;
        }
        PanelAction::Claim => {
            let patterns: Vec<String> = st
                .claim_text
                .split([',', '\n'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            if patterns.is_empty() {
                return;
            }
            let holder = Holder {
                agent: tr("あなた (Zaivern Code)"),
                session: String::new(),
                cwd: normalize_path(&st.roots.tree.to_string_lossy()),
                pid: std::process::id(),
            };
            let now = now_secs();
            let res = with_store(&store_path, |s| {
                try_claim(s, &holder, &patterns, now, DEFAULT_TTL_SECS, &pid_alive)
            });
            st.toast = match res {
                Ok(Claim::Granted(n)) => {
                    st.claim_text.clear();
                    trf("{n} 件のパターンを確保しました", &[("n", n.to_string())])
                }
                Ok(Claim::Refused { owner, pattern, .. }) => trf(
                    "確保できません: 「{pattern}」は {owner} が持っています",
                    &[("pattern", pattern), ("owner", owner)],
                ),
                Err(e) => e,
            };
            st.last_scan = None;
        }
    }
}

/// 非同期の結果を拾い、必要なら次の走査を出す。**待たない**。
fn poll(st: &mut PanelState, ctx: &egui::Context) {
    if let Some(rx) = &st.pending {
        match rx.try_recv() {
            Ok(snap) => {
                match snap.store {
                    Ok(s) => {
                        st.store = s;
                        st.error.clear();
                    }
                    Err(e) => st.error = e,
                }
                st.tier = snap.tier;
                st.last_cost = Some(snap.cost);
                st.last_scan = Some(Instant::now());
                st.pending = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => st.pending = None,
        }
    }
    if st.pending.is_none() {
        // 間隔は所要時間に応じて自動で空ける (遅い環境で走らせ続けない)。
        let due = st
            .last_scan
            .is_none_or(|t| t.elapsed() >= crate::git::scan_interval(SCAN_BASE, st.last_cost));
        if due {
            st.pending = Some(spawn_scan(st.roots.clone()));
        }
    }
    // 開いている間だけ、結果を拾うために軽く回す。
    ctx.request_repaint_after(Duration::from_millis(250));
}

fn body(ui: &mut egui::Ui, st: &mut PanelState) -> PanelAction {
    let mut action = PanelAction::None;
    let tier_now = st.tier;
    let vis = ui.visuals().clone();
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new(format!("● {}", tr(tier_now.label())))
                .color(tier_now.color(&vis))
                .strong(),
        )
        .on_hover_text(tr(tier_now.detail()));
        // worktree のときは 2 つのルートが別物になる。**それを隠さない** —
        // 「なぜ別フォルダの相手と衝突するのか」がここでしか判らない。
        let hover = if st.roots.tree == st.roots.key {
            st.roots.key.display().to_string()
        } else {
            trf(
                "台帳の単位 (元のリポジトリ): {key}\n作業ツリー: {tree}",
                &[
                    ("key", st.roots.key.display().to_string()),
                    ("tree", st.roots.tree.display().to_string()),
                ],
            )
        };
        ui.label(egui::RichText::new(ellipsize(&st.roots.key.to_string_lossy(), 52)).weak())
            .on_hover_text(hover);
        if ui.button("⟳").on_hover_text(tr("読み直す")).clicked() {
            action = PanelAction::Refresh;
        }
    });
    ui.separator();

    if tier_now == Tier::Off {
        // 空状態は**中央に 1 枚のカード**で (CLAUDE.md「空白は作らない」)。
        let avail = ui.available_rect_before_wrap();
        let card = empty_card(avail);
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(card), |ui| {
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(tr("このワークスペースでは無効です")).strong());
                ui.label(
                    egui::RichText::new(tr(
                        "有効にすると、書き込みのたびにファイルの所有を記録します。\nフックを設置してあれば、他人が持つファイルへの書き込みは実際に止まります。",
                    ))
                    .weak(),
                );
                if ui.button(tr("このワークスペースで有効にする")).clicked() {
                    action = PanelAction::Enable;
                }
            });
        });
        toast_line(ui, st);
        return action;
    }

    if !st.error.is_empty() {
        ui.label(egui::RichText::new(st.error.clone()).color(vis.error_fg_color));
    }

    egui::ScrollArea::vertical()
        .id_salt("zv-lease-body")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if let Some(a) = lease_rows(ui, st, &vis) {
                action = a;
            }
            ui.add_space(8.0);
            ui.separator();
            if claim_form(ui, st) {
                action = PanelAction::Claim;
            }
            ui.add_space(8.0);
            ui.separator();
            plan_section(ui, st, &vis);
        });
    toast_line(ui, st);
    action
}

fn toast_line(ui: &mut egui::Ui, st: &PanelState) {
    if !st.toast.is_empty() {
        ui.separator();
        ui.label(egui::RichText::new(st.toast.clone()).weak());
    }
}

fn lease_rows(ui: &mut egui::Ui, st: &PanelState, vis: &egui::Visuals) -> Option<PanelAction> {
    let now = now_secs();
    if st.store.leases.is_empty() {
        ui.label(
            egui::RichText::new(tr(
                "確保中のファイルはありません (エージェントが書き込むと自動で登録されます)",
            ))
            .weak(),
        );
        return None;
    }
    let mut action = None;
    let longest = st
        .store
        .leases
        .iter()
        .map(|l| l.holder.display().chars().count() as f32 * 7.0)
        .fold(40.0f32, f32::max);
    for (i, l) in st.store.leases.iter().enumerate() {
        let w = ui.available_width();
        let row = egui::Rect::from_min_size(ui.next_widget_position(), egui::vec2(w, 20.0));
        let lay = row_layout(row, longest);
        let compact = is_compact(w);
        ui.horizontal(|ui| {
            ui.allocate_ui(egui::vec2(lay.owner.width(), 20.0), |ui| {
                ui.label(ellipsize(&l.holder.display(), 24))
                    .on_hover_text(l.holder.display());
            });
            ui.allocate_ui(egui::vec2(lay.patterns.width(), 20.0), |ui| {
                let joined = l.patterns.join(", ");
                ui.label(egui::RichText::new(ellipsize(&joined, 48)).monospace())
                    .on_hover_text(joined);
            });
            ui.allocate_ui(egui::vec2(lay.left.width(), 20.0), |ui| {
                let left = l.expires_at.saturating_sub(now);
                let txt = crate::instances::humanize_uptime(left);
                let c = if left == 0 {
                    vis.warn_fg_color
                } else {
                    vis.weak_text_color()
                };
                ui.label(egui::RichText::new(txt).color(c))
                    .on_hover_text(tr("この時間を過ぎると自動で解放されます"));
            });
            let label = if compact {
                "✖".to_string()
            } else {
                tr("解放")
            };
            if ui
                .button(label)
                .on_hover_text(tr("このリースを解放する (引き継ぐとき)"))
                .clicked()
            {
                action = Some(PanelAction::Release(i));
            }
        });
    }
    action
}

fn claim_form(ui: &mut egui::Ui, st: &mut PanelState) -> bool {
    let mut go = false;
    ui.label(egui::RichText::new(tr("自分で確保する")).strong());
    ui.horizontal_wrapped(|ui| {
        let w = (ui.available_width() - 120.0).clamp(120.0, 420.0);
        ui.add(
            egui::TextEdit::singleline(&mut st.claim_text)
                .desired_width(w)
                .hint_text(tr("src/auth/**, README.md")),
        );
        if ui
            .button(tr("確保"))
            .on_hover_text(tr("重なりがあれば拒否されます (後勝ちにはしません)"))
            .clicked()
        {
            go = true;
        }
    });
    go
}

fn plan_section(ui: &mut egui::Ui, st: &mut PanelState, vis: &egui::Visuals) {
    ui.label(egui::RichText::new(tr("配る前に重なりを見る")).strong());
    ui.label(
        egui::RichText::new(tr(
            "1 行に「担当: パターン, パターン」。配る前に重なりが判ります",
        ))
        .weak(),
    );
    let w = ui.available_width().max(120.0);
    ui.add(
        egui::TextEdit::multiline(&mut st.plan_text)
            .desired_width(w)
            .desired_rows(3)
            .hint_text("A: src/auth/**\nB: src/ui/**, README.md"),
    );
    let list = parse_assignments(&st.plan_text);
    if list.is_empty() {
        return;
    }
    let ovs = plan_overlaps(&list);
    if ovs.is_empty() {
        ui.label(
            egui::RichText::new(trf(
                "{n} 人の担当は互いに素です。そのまま配れます",
                &[("n", list.len().to_string())],
            ))
            .color(Tier::Enforced.color(vis)),
        );
        return;
    }
    ui.label(
        egui::RichText::new(trf(
            "{n} 件の重なりがあります — このまま配ると、衝突はマージのときまで見えません",
            &[("n", ovs.len().to_string())],
        ))
        .color(vis.warn_fg_color),
    );
    for o in ovs.iter().take(8) {
        let (a, b) = (&list[o.a].agent, &list[o.b].agent);
        ui.label(
            egui::RichText::new(ellipsize(
                &format!("{a} 「{}」 ↔ {b} 「{}」", o.pattern_a, o.pattern_b),
                72,
            ))
            .monospace()
            .weak(),
        );
    }
    let (split, serial) = split_plan(&list);
    ui.label(egui::RichText::new(tr("分割案 (これなら重なりません)")).strong());
    for a in &split {
        let line = if a.patterns.is_empty() {
            trf("{agent}: (割り当て無し)", &[("agent", a.agent.clone())])
        } else {
            format!("{}: {}", a.agent, a.patterns.join(", "))
        };
        ui.label(egui::RichText::new(ellipsize(&line, 72)).monospace());
    }
    if !serial.is_empty() {
        ui.label(
            egui::RichText::new(trf("直列にやる分: {list}", &[("list", serial.join(", "))]))
                .color(vis.warn_fg_color),
        );
    }
}

/// 長い文字列を省略する (全文はホバーで出す)。
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;

    fn dead(_: u32) -> bool {
        false
    }
    fn living(_: u32) -> bool {
        true
    }

    fn holder(agent: &str, session: &str) -> Holder {
        Holder {
            agent: agent.into(),
            session: session.into(),
            cwd: format!("/ws/{agent}"),
            pid: 0,
        }
    }

    // ── パス正規化 ────────────────────────────────────────────────

    #[test]
    fn パスは区切りと余分な要素を正規化する() {
        assert_eq!(normalize_path("src\\app.rs"), "src/app.rs");
        assert_eq!(normalize_path("./src//app.rs"), "src/app.rs");
        assert_eq!(normalize_path("src/"), "src");
        assert_eq!(normalize_path(""), "");
        // 非 ASCII (CJK・空白入り) も壊さない
        assert_eq!(
            normalize_path("ドキュメント/設計 メモ.md"),
            "ドキュメント/設計 メモ.md"
        );
        assert_eq!(
            normalize_path(".\\日本語\\ファイル.rs"),
            "日本語/ファイル.rs"
        );
        // Windows は大文字小文字を畳む。unix は畳まない — **両側を書く**
        if cfg!(windows) {
            assert_eq!(normalize_path("SRC/App.rs"), "src/app.rs");
        } else {
            assert_eq!(normalize_path("SRC/App.rs"), "SRC/App.rs");
        }
    }

    // ── glob の境界 (ここを間違えると全部狂う) ─────────────────────

    #[test]
    fn パターンが具体パスを覆う条件() {
        let table: &[(&str, &str, bool)] = &[
            ("src/app.rs", "src/app.rs", true),
            ("src/app.rs", "src/other.rs", false),
            ("src/**", "src/app.rs", true),
            ("src/**", "src/a/b/c.rs", true),
            ("src/**", "src", true),
            ("src/**", "tests/a.rs", false),
            ("src/", "src/a.rs", true), // 末尾 / はサブツリー
            ("src", "src/a.rs", false), // ディレクトリ名だけでは配下を含まない
            ("src/*.rs", "src/a.rs", true),
            ("src/*.rs", "src/sub/a.rs", false), // * は / を越えない
            ("**/*.rs", "src/a.rs", true),
            ("**/*.rs", "a.rs", true),
            ("**", "何でも/日本語.rs", true),
            ("src/?.rs", "src/a.rs", true),
            ("src/?.rs", "src/ab.rs", false),
            ("ドキュメント/**", "ドキュメント/設計 メモ.md", true),
        ];
        for (pat, path, want) in table {
            assert_eq!(covers(pat, path), *want, "covers({pat:?}, {path:?})");
        }
    }

    #[test]
    fn パターン同士の重なり判定() {
        let table: &[(&str, &str, bool)] = &[
            ("src/**", "src/a.rs", true),
            ("src/a.rs", "src/a.rs", true),
            ("src/a.rs", "src/b.rs", false),
            // ファイルとその親ディレクトリ
            ("src/", "src/a.rs", true),
            ("src/**", "src/sub/", true),
            // 兄弟は重ならない
            ("src/auth/**", "src/ui/**", false),
            ("src/auth/**", "src/auth/x/y.rs", true),
            // ワイルドカード同士
            ("src/*.rs", "src/a*", true),
            ("src/*.rs", "src/*.md", false),
            ("**/*.rs", "src/**", true),
            ("**", "何でも", true),
            ("a/**/z.rs", "a/b/c/z.rs", true),
            ("a/**/z.rs", "a/b/c/y.rs", false),
            // ** は 0 個のセグメントにも当たる
            ("a/**", "a", true),
            ("a/**/b", "a/b", true),
            // CJK
            ("ドキュメント/**", "ドキュメント/設計.md", true),
            ("ドキュメント/**", "資料/設計.md", false),
        ];
        for (a, b, want) in table {
            assert_eq!(overlaps(a, b), *want, "overlaps({a:?}, {b:?})");
            assert_eq!(overlaps(b, a), *want, "対称でない: ({a:?}, {b:?})");
        }
    }

    #[test]
    fn ワイルドカードが並んでも爆発しない() {
        // 素朴な再帰なら指数になる形。DP なので即返る。
        let a = "*a*a*a*a*a*a*a*a*a*a*";
        let b = "*b*b*b*b*b*b*b*b*b*b*";
        let t0 = Instant::now();
        assert!(overlaps(a, b), "どちらも任意文字列に当たるので重なる");
        assert!(
            t0.elapsed() < Duration::from_millis(200),
            "{:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn 覆う関係なら重なる_無ワイルドカードでは一致する() {
        for (pat, path) in [
            ("src/**", "src/a.rs"),
            ("src/a.rs", "src/a.rs"),
            ("**/*.rs", "x/y/z.rs"),
        ] {
            assert!(covers(pat, path));
            assert!(overlaps(pat, path), "覆うなら必ず重なる");
        }
        assert!(!covers("src/a.rs", "src/b.rs"));
        assert!(!overlaps("src/a.rs", "src/b.rs"));
    }

    // ── スコープ ─────────────────────────────────────────────────

    #[test]
    fn worktree_のポインタから元のリポジトリへ寄せる() {
        let got = main_repo_root_from_pointer("gitdir: /repos/proj/.git/worktrees/feat-a\n");
        assert_eq!(got, Some(PathBuf::from("/repos/proj")));
        // Windows 形式
        let got = main_repo_root_from_pointer("gitdir: C:/r/p/.git/worktrees/w1");
        assert_eq!(got, Some(PathBuf::from("C:/r/p")));
        // 形が違えば推測しない
        assert_eq!(
            main_repo_root_from_pointer("gitdir: /repos/proj/.git"),
            None
        );
        assert_eq!(main_repo_root_from_pointer("これは違う"), None);
    }

    #[test]
    fn git_の無いフォルダでもスコープが決まる() {
        let dir = unique_temp_dir("zaivern", "lease-nogit");
        let sub = dir.join("a/b");
        std::fs::create_dir_all(&sub).expect("mkdir");
        // .git が無ければその場所自身。パニックしない
        let r = roots_of(&sub);
        let want = sub.canonicalize().unwrap_or_else(|_| sub.clone());
        assert_eq!(r.key, want);
        assert_eq!(r.tree, want, "git 管理外では 2 つのルートが一致する");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worktree_は元のリポジトリと同じ台帳を引く() {
        let base = unique_temp_dir("zaivern", "lease-worktree");
        let main = base.join("proj");
        std::fs::create_dir_all(main.join(".git/worktrees/w1")).expect("mkdir");
        std::fs::create_dir_all(main.join("src")).expect("mkdir");
        let wt = base.join("wt-1/src");
        std::fs::create_dir_all(&wt).expect("mkdir");
        std::fs::write(
            base.join("wt-1/.git"),
            format!("gitdir: {}/.git/worktrees/w1\n", main.display()),
        )
        .expect("write");
        // **ここが競合との差**: worktree でも台帳のキーは元のリポジトリへ寄る
        let a = roots_of(&main.join("src"));
        let b = roots_of(&wt);
        assert_eq!(a.key, b.key, "worktree が別スコープになると衝突を防げない");
        let dir = base.join("leases");
        assert_eq!(store_path_in(&dir, &a.key), store_path_in(&dir, &b.key));
        // **相対化は作業ツリー基準**。ここを key 基準にすると worktree の
        // ファイルが 1 つも当たらず、機能が無言で死ぬ (実際に踏んだ)。
        assert_ne!(a.tree, b.tree, "作業ツリーは別物のはず");
        assert_eq!(
            rel_within(&b.tree, &wt.join("a.rs")),
            Some("src/a.rs".to_string()),
            "worktree のファイルは worktree 基準で相対化する"
        );
        assert_eq!(
            rel_within(&a.key, &wt.join("a.rs")),
            None,
            "元リポジトリ基準では当たらない (これが e2e で踏んだ穴)"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn スコープ外のパスは関知しない() {
        let dir = unique_temp_dir("zaivern", "lease-outside");
        let scope = dir.join("ws");
        std::fs::create_dir_all(scope.join("src")).expect("mkdir");
        std::fs::create_dir_all(dir.join("other")).expect("mkdir");
        assert_eq!(
            rel_within(&scope, &scope.join("src")),
            Some("src".to_string())
        );
        assert_eq!(rel_within(&scope, &dir.join("other")), None);
        assert_eq!(rel_within(&scope, &scope), None, "スコープ自身は対象外");
        // **まだ無いファイル** (Write で新規作成される側) も相対化できること。
        // ここが外れると新規ファイルの衝突を 1 件も止められない。
        assert_eq!(
            rel_within(&scope, &scope.join("src/まだ無い.rs")),
            Some("src/まだ無い.rs".to_string())
        );
        assert_eq!(
            rel_within(&scope, &scope.join("新しい階層/深い/file.rs")),
            Some("新しい階層/深い/file.rs".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 確保 (fail-closed) ────────────────────────────────────────

    #[test]
    fn 競合したら片方だけが勝つ() {
        let mut s = Store::default();
        let a = holder("A", "s-a");
        let b = holder("B", "s-b");
        assert_eq!(
            try_claim(&mut s, &a, &["src/**".into()], 100, 600, &dead),
            Claim::Granted(1)
        );
        // 重なるものは**取れない** (後勝ちにしない)
        match try_claim(&mut s, &b, &["src/app.rs".into()], 100, 600, &dead) {
            Claim::Refused { owner, pattern, .. } => {
                assert!(owner.contains('A'), "{owner}");
                assert_eq!(pattern, "src/**");
            }
            other => panic!("競合を通してしまった: {other:?}"),
        }
        // 重ならないものは取れる
        assert_eq!(
            try_claim(&mut s, &b, &["docs/**".into()], 100, 600, &dead),
            Claim::Granted(1)
        );
        assert_eq!(s.leases.len(), 2);
    }

    #[test]
    fn 一部でも重なれば一つも取らない() {
        let mut s = Store::default();
        let a = holder("A", "s-a");
        let b = holder("B", "s-b");
        try_claim(&mut s, &a, &["src/**".into()], 0, 600, &dead);
        let before = s.clone();
        let r = try_claim(
            &mut s,
            &b,
            &["docs/x.md".into(), "src/a.rs".into()],
            0,
            600,
            &dead,
        );
        assert!(matches!(r, Claim::Refused { .. }));
        assert_eq!(s, before, "部分的に取れてはいけない (全か無か)");
    }

    #[test]
    fn 同じ持ち主なら追加で取れて期限が伸びる() {
        let mut s = Store::default();
        let a = holder("A", "s-a");
        try_claim(&mut s, &a, &["src/a.rs".into()], 0, 600, &dead);
        assert_eq!(
            try_claim(
                &mut s,
                &a,
                &["src/a.rs".into(), "src/b.rs".into()],
                300,
                600,
                &dead
            ),
            Claim::Granted(1),
            "既に持っているパターンは数えない"
        );
        assert_eq!(s.leases.len(), 1);
        assert_eq!(s.leases[0].patterns, vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(s.leases[0].expires_at, 900);
    }

    #[test]
    fn セッションidが違えば同じフォルダでも別人() {
        let mut s = Store::default();
        let mut a = holder("claude", "s-1");
        let mut b = holder("claude", "s-2");
        a.cwd = "/ws".into();
        b.cwd = "/ws".into();
        try_claim(&mut s, &a, &["src/a.rs".into()], 0, 600, &dead);
        assert!(matches!(
            try_claim(&mut s, &b, &["src/a.rs".into()], 0, 600, &dead),
            Claim::Refused { .. }
        ));
    }

    // ── 期限と安全な回収 ──────────────────────────────────────────

    #[test]
    fn 期限切れは回収される() {
        let mut s = Store::default();
        try_claim(
            &mut s,
            &holder("A", "s-a"),
            &["src/**".into()],
            0,
            600,
            &dead,
        );
        prune(&mut s, 599, &dead);
        assert_eq!(s.leases.len(), 1, "期限内は残る");
        prune(&mut s, 601, &dead);
        assert!(s.leases.is_empty(), "期限切れは回収する");
    }

    #[test]
    fn 生きている持ち主には猶予があるが上限がある() {
        let mut s = Store::default();
        let h = Holder {
            pid: 4242,
            ..holder("A", "s-a")
        };
        try_claim(&mut s, &h, &["src/**".into()], 0, 600, &living);
        // 期限切れでも、プロセスが生きている間は猶予
        assert!(
            s.leases[0].active(700, &living),
            "戻ってきた本人から奪わない"
        );
        // **上限がある** (PID 再利用で永久に残らない)
        assert!(
            !s.leases[0].active(600 + RECLAIM_GRACE_SECS + 1, &living),
            "猶予に上限が無いと、再利用された PID で永久に人質になる"
        );
        // 死んでいれば猶予なし
        assert!(!s.leases[0].active(601, &dead));
    }

    #[test]
    fn pid_を持たないリースは_ttl_だけで回収する() {
        let mut s = Store::default();
        try_claim(&mut s, &holder("A", "s-a"), &["x".into()], 0, 10, &living);
        assert_eq!(s.leases[0].holder.pid, 0);
        assert!(!s.leases[0].active(11, &living), "PID が無ければ猶予もない");
    }

    // ── 判定 (フックの心臓) ───────────────────────────────────────

    #[test]
    fn 判定表_自分は通り他人は止まり未所有は通る() {
        let mut s = Store::default();
        let a = holder("A", "s-a");
        let b = holder("B", "s-b");
        try_claim(&mut s, &a, &["src/**".into()], 100, 600, &dead);
        assert_eq!(decide(&s, &a, "src/app.rs", 100, &dead), Verdict::Allow);
        let Verdict::Deny(reason) = decide(&s, &b, "src/app.rs", 100, &dead) else {
            panic!("他人の所有を通してしまった");
        };
        // 理由は**行動できる**内容であること
        assert!(reason.contains("src/app.rs"), "{reason}");
        assert!(reason.contains('A'), "誰が持っているかが無い: {reason}");
        assert!(reason.contains("待つ"), "どうすればよいかが無い: {reason}");
        // 未所有は通る
        assert_eq!(decide(&s, &b, "docs/x.md", 100, &dead), Verdict::Allow);
        // 期限切れも通る
        assert_eq!(decide(&s, &b, "src/app.rs", 9_999, &dead), Verdict::Allow);
    }

    #[test]
    fn 壊れた台帳は_fail_open_で許可する() {
        let dir = unique_temp_dir("zaivern", "lease-broken");
        let store = dir.join("broken.json");
        std::fs::write(&store, "{ これは JSON ではない").expect("write");
        assert!(read_store(&store).is_err(), "壊れているのは検知する");
        // gate は内部エラーで通す (自分のバグでユーザーを止めない)
        let payload = serde_json::json!({
            "session_id": "s-x",
            "cwd": dir.to_string_lossy(),
            "hook_event_name": "PreToolUse",
            "tool_name": "Edit",
            "tool_input": { "file_path": dir.join("a.rs").to_string_lossy() },
        })
        .to_string();
        // 台帳の場所は workspace_key 由来なので、この壊れたファイルとは別。
        // ここでは「読めない入力でも panic しない」ことを見る。
        assert_eq!(gate("claude", "PreToolUse", &payload).exit, 0);
        assert_eq!(
            gate("claude", "PreToolUse", "これは JSON ではない"),
            pass_answer()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 台帳の入出力 ─────────────────────────────────────────────

    #[test]
    fn 台帳は書いて読み戻せる() {
        let dir = unique_temp_dir("zaivern", "lease-io");
        let store = dir.join("s.json");
        assert!(!enabled(&store));
        enable(&store).expect("有効化");
        assert!(enabled(&store));
        with_store(&store, |s| {
            try_claim(s, &holder("A", "s-a"), &["src/**".into()], 5, 600, &dead)
        })
        .expect("確保");
        let got = read_store(&store).expect("読める");
        assert_eq!(got.leases.len(), 1);
        assert_eq!(got.leases[0].patterns, vec!["src/**"]);
        assert_eq!(got.leases[0].acquired_at, 5);
        // 解放
        let n = with_store(&store, |s| release(s, &holder("A", "s-a"))).expect("解放");
        assert_eq!(n, 1);
        assert!(read_store(&store).expect("読める").leases.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ロックは同時に一つしか取れない() {
        let dir = unique_temp_dir("zaivern", "lease-lock");
        let store = dir.join("s.json");
        enable(&store).expect("有効化");
        let g = acquire_lock(&store).expect("1 つ目は取れる");
        let t0 = Instant::now();
        assert!(acquire_lock(&store).is_err(), "2 つ目が取れてはいけない");
        assert!(
            t0.elapsed() < Duration::from_millis(LOCK_WAIT_MS + 300),
            "待ち過ぎ: {:?}",
            t0.elapsed()
        );
        drop(g);
        assert!(acquire_lock(&store).is_ok(), "解放後は取れる");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 置き去りのロックは奪える() {
        let dir = unique_temp_dir("zaivern", "lease-stale-lock");
        let store = dir.join("s.json");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let lock = store.with_extension("lock");
        std::fs::write(&lock, b"").expect("write");
        // mtime を過去へ倒せない環境もあるので、TTL 判定そのものを検証する。
        let old = std::time::SystemTime::now() - Duration::from_millis(LOCK_STALE_MS * 2);
        let ok = filetime_set(&lock, old);
        if ok {
            assert!(acquire_lock(&store).is_ok(), "古いロックは奪える");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// mtime を過去へ倒す。`File::set_times` は Rust 1.75 で安定した
    /// **移植可能な**手段 (libc へ降りない)。使えない環境では `false`。
    fn filetime_set(path: &Path, when: std::time::SystemTime) -> bool {
        let Ok(f) = std::fs::File::options().write(true).open(path) else {
            return false;
        };
        f.set_times(std::fs::FileTimes::new().set_modified(when))
            .is_ok()
    }

    #[test]
    fn 二つのプロセスが競っても片方しか取れない() {
        // 同一プロセス内の 2 スレッドで、実ファイルのロックを取り合う。
        let dir = unique_temp_dir("zaivern", "lease-race");
        let store = dir.join("s.json");
        enable(&store).expect("有効化");
        let (s1, s2) = (store.clone(), store.clone());
        let h1 = std::thread::spawn(move || {
            with_store(&s1, |s| {
                try_claim(s, &holder("A", "s-a"), &["src/**".into()], 0, 600, &dead)
            })
        });
        let h2 = std::thread::spawn(move || {
            with_store(&s2, |s| {
                try_claim(
                    s,
                    &holder("B", "s-b"),
                    &["src/app.rs".into()],
                    0,
                    600,
                    &dead,
                )
            })
        });
        let r1 = h1.join().expect("join");
        let r2 = h2.join().expect("join");
        let granted = [&r1, &r2]
            .iter()
            .filter(|r| matches!(r, Ok(Claim::Granted(_))))
            .count();
        // 少なくとも片方は取れ、台帳の中身は 1 人ぶんだけ (後勝ちが起きない)
        assert!(granted >= 1, "{r1:?} {r2:?}");
        let got = read_store(&store).expect("読める");
        assert_eq!(
            got.leases.len(),
            1,
            "2 人が同時に所有してはいけない: {got:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── フックの応答 ─────────────────────────────────────────────

    #[test]
    fn 拒否の応答はベンダーのスキーマに一致する() {
        let a = deny_answer("だめ");
        let v: serde_json::Value = serde_json::from_str(&a.stdout).expect("JSON");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(v["hookSpecificOutput"]["permissionDecisionReason"], "だめ");
        assert_eq!(a.exit, 0, "JSON は exit 0 のときだけ読まれる");
        // 許可では**何も出さない** ("allow" はユーザーの許可設定を飛び越える)
        assert!(pass_answer().stdout.is_empty());
    }

    #[test]
    fn ペイロードから書き込み先を取り出す() {
        let keys = ["file_path", "notebook_path"];
        let v: serde_json::Value =
            serde_json::from_str(r#"{"tool_input":{"file_path":"/a/b.rs"}}"#).expect("JSON");
        assert_eq!(target_path(&v, &keys), "/a/b.rs");
        let v: serde_json::Value =
            serde_json::from_str(r#"{"tool_input":{"notebook_path":"/a/n.ipynb"}}"#).expect("JSON");
        assert_eq!(target_path(&v, &keys), "/a/n.ipynb");
        let v: serde_json::Value =
            serde_json::from_str(r#"{"tool_input":{"edits":[{"file_path":"/a/c.rs"}]}}"#)
                .expect("JSON");
        assert_eq!(target_path(&v, &keys), "/a/c.rs");
        let v: serde_json::Value = serde_json::from_str(r#"{"tool_input":{}}"#).expect("JSON");
        assert_eq!(target_path(&v, &keys), "");
    }

    #[test]
    fn 書き込み以外のツールとイベントは素通しする() {
        let payload = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Read",
            "tool_input": { "file_path": "/x/y.rs" },
        })
        .to_string();
        assert_eq!(gate("claude", "PreToolUse", &payload), pass_answer());
        assert_eq!(gate("claude", "PostToolUse", &payload), pass_answer());
        // カタログに無いエージェントも素通し
        assert_eq!(gate("未知のCLI", "PreToolUse", &payload), pass_answer());
    }

    #[test]
    fn 台帳が無いワークスペースでは何もしない() {
        let dir = unique_temp_dir("zaivern", "lease-off");
        std::fs::create_dir_all(dir.join("src")).expect("mkdir");
        let payload = serde_json::json!({
            "session_id": "s-1",
            "cwd": dir.to_string_lossy(),
            "hook_event_name": "PreToolUse",
            "tool_name": "Edit",
            "tool_input": { "file_path": dir.join("src/a.rs").to_string_lossy() },
        })
        .to_string();
        assert_eq!(gate("claude", "PreToolUse", &payload), pass_answer());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 事前の重複検出 ───────────────────────────────────────────

    #[test]
    fn 事前に重なりを見つけて分割案を出す() {
        let list = parse_assignments("A: src/**\nB: src/auth/x.rs, docs/**\n# コメント\n");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].agent, "A");
        assert_eq!(list[1].patterns, vec!["src/auth/x.rs", "docs/**"]);
        let ovs = plan_overlaps(&list);
        assert_eq!(ovs.len(), 1, "{ovs:?}");
        assert_eq!(ovs[0].pattern_a, "src/**");
        let (split, serial) = split_plan(&list);
        assert_eq!(split[0].patterns, vec!["src/**"]);
        assert_eq!(split[1].patterns, vec!["docs/**"], "重なる分は外れる");
        assert_eq!(serial, vec!["src/auth/x.rs"]);
        // 分割後は 1 件も重ならない
        assert!(plan_overlaps(&split).is_empty());
    }

    #[test]
    fn 互いに素な割り当ては警告しない() {
        let list = parse_assignments("A: src/auth/**\nB: src/ui/**\nC: README.md");
        assert!(plan_overlaps(&list).is_empty());
        let (split, serial) = split_plan(&list);
        assert_eq!(split, list, "重なりが無ければ何も削らない");
        assert!(serial.is_empty());
    }

    // ── 段 ──────────────────────────────────────────────────────

    /// 段の色が全テーマの背景に対して読める (WCAG AA 大文字 = 3.0 以上)。
    ///
    /// `Tier::Enforced` だけは egui の `Visuals` に対応する色が無いので
    /// 明暗 2 通りを直接持っている。**持つなら検算する**。
    #[test]
    fn 段の色はどのテーマでも読める() {
        for t in crate::theme::all() {
            let mut v = if t.dark {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            };
            v.warn_fg_color = t.warn;
            let c = Tier::Enforced.color(&v);
            let ratio = crate::theme::contrast_ratio(c, t.panel);
            assert!(
                ratio >= 3.0,
                "{}: 強制の色 {c:?} が背景 {:?} に対して {ratio:.2} しかない",
                t.name,
                t.panel
            );
        }
    }

    #[test]
    fn 段は正直に出す() {
        assert_eq!(tier(true, true), Tier::Enforced);
        assert_eq!(tier(true, false), Tier::Advisory);
        assert_eq!(tier(false, true), Tier::Off);
        assert_eq!(tier(false, false), Tier::Off);
        for t in [Tier::Enforced, Tier::Advisory, Tier::Off] {
            assert!(!t.label().is_empty());
            assert!(!t.detail().is_empty());
        }
    }

    // ── レイアウト (極端な寸法) ───────────────────────────────────

    fn area(w: f32, h: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h))
    }

    #[test]
    fn 行のレイアウトはどの幅でも収まり重ならない() {
        for (w, h) in [
            (900.0f32, 700.0f32),
            (1200.0, 300.0),
            (320.0, 240.0),
            (120.0, 60.0),
        ] {
            for longest in [40.0f32, 120.0, 600.0] {
                let row = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, 20.0));
                let lay = row_layout(row, longest);
                let rects = [lay.owner, lay.patterns, lay.left, lay.actions];
                for (i, r) in rects.iter().enumerate() {
                    assert!(r.width() >= 0.0, "{w}x{h} 列 {i} の幅が負");
                    assert!(
                        r.left() >= row.left() - 0.01 && r.right() <= row.right() + 0.01,
                        "{w}x{h} 列 {i} が領域外: {r:?} / {row:?}"
                    );
                }
                for i in 1..rects.len() {
                    assert!(
                        rects[i].left() >= rects[i - 1].right() - 0.01,
                        "{w}x{h} 列 {i} が前の列と重なる: {:?} {:?}",
                        rects[i - 1],
                        rects[i]
                    );
                }
            }
        }
    }

    #[test]
    fn 狭いときはボタンがアイコンだけになる() {
        assert!(is_compact(400.0));
        assert!(!is_compact(900.0));
        // 1200x300 は横に広いので縮退しない (縦の狭さは列幅に効かない)
        assert!(!is_compact(1200.0));
    }

    #[test]
    fn 空状態のカードは中央に一枚で領域内に収まる() {
        for (w, h) in [(900.0f32, 700.0f32), (1200.0, 300.0), (200.0, 100.0)] {
            let a = area(w, h);
            let c = empty_card(a);
            assert!(a.contains_rect(c), "{w}x{h}: カードがはみ出す {c:?}");
            assert!(
                (c.center().x - a.center().x).abs() < 0.01
                    && (c.center().y - a.center().y).abs() < 0.01,
                "{w}x{h}: 中央に置くこと"
            );
        }
    }

    #[test]
    fn 長い文字列は省略してホバーへ回す() {
        assert_eq!(ellipsize("abc", 5), "abc");
        assert_eq!(ellipsize("abcdefg", 5), "abcd…");
        // 文字単位で切る (CJK でバイト境界を割らない)
        assert_eq!(ellipsize("日本語のながい名前", 4), "日本語…");
    }
}
