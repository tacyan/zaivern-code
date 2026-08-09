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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
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

/// ロック待ちが尽きたことを示す接頭辞 (表示はされない制御文字)。
/// [`is_lock_busy`] だけが読む。
const LOCK_BUSY: &str = "\u{0}busy:";

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
    // 規則は 3 つの OS ぶんあるが、動いている OS のぶんしか実行されない。
    // **引数へ出しておかないと、macOS で開発している限り Windows / Linux の
    // 規則は一度も検査されない** (`keybinds::canonical_mods_on` と同じ流儀)。
    normalize_path_on(raw, true, cfg!(any(windows, target_os = "macos")))
}

/// 規則を明示する [`normalize_path`]。
///
/// * `win_sep` — `\` も区切りとして畳むか (Windows 由来のパスを受けるため)
/// * `fold_case` — 大文字小文字を畳むか
///
/// 固定すべき表は Windows=(true, true) / macOS=(true, true) / Linux=(true, false)。
/// **どのホストからでも 3 通り全部をテストできる**ようにするのがこの関数の目的。
pub fn normalize_path_on(raw: &str, win_sep: bool, fold_case: bool) -> String {
    let slashed = if win_sep {
        raw.replace('\\', "/")
    } else {
        raw.to_string()
    };
    // **末尾の `/` は「その配下ぜんぶ」の意味**。ここで落とすと
    // `zai lease claim src/` が `src` という 1 ファイルの確保になり、
    // **配下を 1 つも守らないのに「確保しました」と返る** (実測で踏んだ。
    // 人が担当表を書くときの最も自然な書き方が no-op になっていた)。
    let subtree = slashed.ends_with('/');
    let mut segs: Vec<&str> = Vec::new();
    for seg in slashed.split('/') {
        match seg {
            "" | "." => continue,
            // `..` を畳む。畳まないと `src/sub/../mod.rs` が
            // `src/sub/../mod.rs` のまま台帳に載り、実際の
            // `src/mod.rs` への書き込みと一致しない (確保側だけずれる)。
            ".." => {
                // 先頭を越える `..` は落とす (スコープ相対なので外は関知しない)。
                segs.pop();
            }
            _ => segs.push(seg),
        }
    }
    let mut out = segs.join("/");
    if subtree && !out.is_empty() {
        out.push_str("/**");
    }
    // **大小非区別は「OS」ではなく「ボリューム」の性質**だが、macOS の既定
    // (APFS) と Windows はどちらも非区別なので、この 2 つは畳む。
    // ここを `cfg!(windows)` だけにしていたため、**開発機である macOS で
    // `src/Foo.rs` と `src/foo.rs` が別リースになり、同じ物理ファイルへ
    // 2 人が同時に書けていた** (実バイナリで再現済み)。
    // 畳みすぎる側は「別ファイルを同じ扱いにする」= 過剰に止める方向なので
    // fail-closed。取りこぼす側と違って衝突は生まない。
    if fold_case {
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
    // **`**` が複数あると素の再帰は組合せ爆発する。** 実測で `**` 8 個の
    // パターン 1 件の判定に 35 秒かかった = 書き込みの臨界路が丸ごと止まる。
    // (状態は「パターンの何番目 × パスの何番目」しか無いので、一度調べた
    //  組を覚えるだけで O(|pat|×|path|) に落ちる。)
    let mut seen = vec![false; (pat.len() + 1) * (path.len() + 1)];
    seg_covers_memo(pat, path, 0, 0, path.len() + 1, &mut seen)
}

fn seg_covers_memo(
    pat: &[String],
    path: &[String],
    pi: usize,
    si: usize,
    stride: usize,
    seen: &mut Vec<bool>,
) -> bool {
    let key = pi * stride + si;
    if seen[key] {
        // この (pi, si) は別の経路で調べ済み。偽だったから戻ってきている。
        return false;
    }
    seen[key] = true;
    let Some(head) = pat.get(pi) else {
        return si == path.len();
    };
    if head == "**" {
        return (si..=path.len()).any(|k| seg_covers_memo(pat, path, pi + 1, k, stride, seen));
    }
    let Some(seg) = path.get(si) else {
        return false;
    };
    seg_one(head, seg) && seg_covers_memo(pat, path, pi + 1, si + 1, stride, seen)
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
/// `Path::is_absolute` は**動いている OS の規則**でしか判定しない。
/// Windows で作られた `.git` を unix 側から読むと `C:/…` が「相対」に見え、
/// 基準ディレクトリを頭に足してしまう。両方の綴りを絶対として扱う。
fn is_absolute_any(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/')
        || p.starts_with('\\')
        || (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic())
}

pub fn main_repo_root_from_pointer(text: &str, dot_dir: &Path) -> Option<PathBuf> {
    let line = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("gitdir:"))?;
    let raw = PathBuf::from(line.trim());
    // **`gitdir:` は相対で書かれることがある** (submodule は git が常に相対で
    // 書く)。基準は「`.git` ファイルが置かれているディレクトリ」であって
    // プロセスの作業フォルダではない。ここを取り違えると、`cwd` が `/` の
    // ときにキーが `/` という**全世界共通のバケツ**になる (実測で踏んだ)。
    let gitdir = if is_absolute_any(line.trim()) {
        raw
    } else {
        dot_dir.join(raw)
    };
    // git は linked worktree の gitdir に必ず `commondir` を置く。中身は
    // 共有 git ディレクトリへの (多くは相対) パス。
    // **`.git` という名前を決め打ちしない**のが肝で、bare リポジトリから
    // 生やした worktree では共有側が `.git` ではない。決め打ちしていたため
    // **並列エージェント運用で最も勧められる「bare + worktree 群」で保証が
    // 丸ごと消えていた** (しかも無言で)。
    let common = std::fs::read_to_string(gitdir.join("commondir"))
        .ok()
        .map(|t| {
            let t = t.trim().to_string();
            if Path::new(&t).is_absolute() {
                PathBuf::from(t)
            } else {
                gitdir.join(t)
            }
        });
    let Some(common) = common else {
        // `commondir` が読めない = git が置いた worktree ではない (あるいは
        // 非常に古い git)。**ここで形を推測しない** — 従来どおり
        // `…/.git/worktrees/<名前>` の形にだけ合わせ、違えば `None` を返す。
        // 緩めると、worktree でもない `.git` ファイルから見当違いの
        // ルートを作ってしまう。
        let git = gitdir.parent()?.parent()?;
        if git.file_name().and_then(|s| s.to_str()) != Some(".git") {
            return None;
        }
        return git.parent().map(Path::to_path_buf);
    };
    let common = canonical_best_effort(&common);
    // 共有 git ディレクトリが `.git` なら**その親**が作業リポジトリのルート。
    // そうでなければ (bare) 共有ディレクトリ自身をキーにする。
    if common.file_name().and_then(|s| s.to_str()) == Some(".git") {
        common.parent().map(Path::to_path_buf)
    } else {
        Some(common)
    }
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
                .and_then(|t| main_repo_root_from_pointer(&t, dir))
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
        // 台帳が上限に達した。**ここを「許可」に倒してはいけない** —
        // 以前は `Granted(0)` を返していたが、それは「確保できていないのに
        // 取れたと言う」ことで、以後**全員が同じファイルへ通ってしまう**
        // (敵対的検証で実際に破られた)。上限は壊れた書き手への防壁なので、
        // 防壁に当たったら止めて人に知らせるのが正しい。
        return Claim::Refused {
            owner: tr("台帳が上限に達しています"),
            pattern: tr(
                "(台帳の掃除が必要です: zai lease list で確認し、不要な確保を解放してください)",
            ),
            until: now,
        };
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

/// エラーが「混んでいて登録できなかった」か (= 再試行で直る)。
pub fn is_lock_busy(e: &str) -> bool {
    e.starts_with(LOCK_BUSY)
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
            // **文面ではなく接頭辞で識別する** (文面は翻訳で変わる)。
            // 呼び出し側は「混んでいて登録できなかった」と
            // 「台帳が壊れている」を区別しなければならない — 前者は
            // 再試行すれば直るので止めてよく、後者は止めると詰む。
            return Err(format!(
                "{LOCK_BUSY}{}",
                tr("ロックを取れませんでした (先客が握っています)")
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// ロックを取って読み → 変更 → 書き戻す。**確保の唯一の入口**。
///
/// **変化が無ければ書かない。** ここが並列度の効く場所で、以前は
/// 「許可されるだけ」の呼び出しでも tmp 書き + rename を払っていた。
/// ロックの保持時間がそのぶん伸び、**16 体を並べるとロック待ち
/// ([`LOCK_WAIT_MS`]) を超えて fail-open し、本物の衝突が漏れた**
/// (計測で実際に踏んだ: 128 回中 1 回・B 群に 1 ハンク)。
/// 判定だけの呼び出しはロックを読みっぱなしで抜けるので、保持時間は
/// 小さな JSON の読み取りだけになる。
pub fn with_store<T>(store: &Path, f: impl FnOnce(&mut Store) -> T) -> Result<T, String> {
    let _lock = acquire_lock(store)?;
    let mut s = read_store(store)?;
    let before = s.clone();
    let out = f(&mut s);
    if s != before {
        write_store(store, &s)?;
    }
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
pub fn deny_answer(agent: &str, reason: &str) -> HookAnswer {
    // **拒否の形はベンダーごとに違う。** カタログから引き、無ければ
    // Claude の形へ落とす (未知のエージェントで無反応にならないように)。
    // 文字列連結ではなく serde で組むのがカタログ側の約束 — 理由に `"` や
    // 改行が入ると壊れた JSON になり、**拒否が黙って無視される**ため。
    if let Some((stdout, exit)) = crate::agents::deny_payload(agent, reason) {
        return HookAnswer {
            stdout,
            stderr: reason.to_string(),
            exit,
        };
    }
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
    // イベント名もベンダーごとに違う (gemini は `BeforeTool`)。
    // 未知のエージェントは `None` が返るのでここで抜ける = 従来と同じ。
    if Some(event.as_str()) != crate::agents::hook_gate_event(agent) {
        return pass_answer();
    }
    if crate::agents::hook_target(agent).is_none() {
        return pass_answer(); // カタログに無いエージェント = 形が判らない
    }
    // 「書き込み系ツールか」もカタログから引く (ここにツール名を書かない)。
    //
    // **パス型 (`Edit`/`Write`) だけでなくコマンド型 (`Bash`) も通す。**
    // 以前はパス型しか見ておらず、`printf X > shared.rs` のような
    // シェル経由の書き込みが**丸ごと素通り**していた (敵対的検証で実際に
    // 上書きされた)。エージェントは `sed -i` / リダイレクトで日常的に書く。
    let tool = s("tool_name");
    let editing = crate::agents::hook_tool_state(agent, &tool)
        == Some(crate::supervisor::protocol::ProtoState::Editing);
    if !editing && crate::agents::hook_command_key(agent, &tool).is_none() {
        // `ls` や `cargo test` はここで抜ける (stat すら踏まない)。
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
    let holder = Holder {
        agent: agent.to_string(),
        session: s("session_id"),
        cwd: normalize_path(&cwd.to_string_lossy()),
        pid: 0, // フックは短命プロセス。生存確認には使えないので TTL に委ねる
    };
    // 書き込み先の抽出は**有効なワークスペースでだけ**払う (コマンド行の
    // 解析は stat より高いので、使っていない人に持たせない)。
    let write = crate::agents::hook_write_targets(agent, &tool, &v);
    if write.opaque {
        // 書き込みらしいのに宛先が判らない (`eval` / 変数展開 / ヒアドキュメント)。
        // **止めない** — `ls` まで落ちる作りにするとユーザーは機能ごと切り、
        // 切られた機能の保証はゼロになる。監査に残して明示リースで守る。
        log_line(&dir, &format!("opaque-write {}", holder.display()));
    }
    if write.paths.is_empty() {
        return pass_answer();
    }
    // 相対パスへ。**相対化は作業ツリー基準**で行う (worktree のファイルは
    // 元のリポジトリの配下に無いので、key 基準にすると必ず外れる)。
    // ツリーの外 (別リポジトリ・システムのファイル) は関知しない。
    let rels: Vec<String> = write
        .paths
        .iter()
        .map(|raw| {
            if Path::new(raw).is_absolute() {
                PathBuf::from(raw)
            } else {
                cwd.join(raw)
            }
        })
        .filter_map(|abs| rel_within(&roots.tree, &abs))
        .collect();
    if rels.is_empty() {
        return pass_answer();
    }
    let now = now_secs();
    let alive: &dyn Fn(u32) -> bool = &pid_alive;

    // **拒否はロックを取らずに決める。** 台帳の置き換えは tmp → rename
    // なので、ロック無しで読んでも書きかけは見えない。ここでロックを待つと、
    // 並列度が上がったときに「待ちきれず fail-open して衝突が漏れる」—
    // つまり**いちばん混んでいるとき (= いちばん衝突しやすいとき) にだけ
    // 効かなくなる**という最悪の壊れ方をする。実測で踏んだ穴なので塞ぐ。
    if let Ok(st) = read_store(&store) {
        // **1 件でも他人が持っていたら止める。** 1 コマンドが複数のファイルを
        // 書く (`mv a b` / `sed -i f1 f2`) ので、部分的に通すと「取れたと
        // 思って書き始めて、途中で衝突する」いちばん危ない形になる。
        for rel in &rels {
            if let Verdict::Deny(reason) = decide(&st, &holder, rel, now, alive) {
                log_line(&dir, &format!("deny {} {rel}", holder.display()));
                return deny_answer(agent, &reason);
            }
        }
    }

    // 通す側だけロックを取り、「判定 → 自動確保 / 延長」を済ませる。
    let outcome = with_store(&store, |st| {
        prune(st, now, alive);
        for rel in &rels {
            if let Verdict::Deny(reason) = decide(st, &holder, rel, now, alive) {
                return Verdict::Deny(reason);
            }
        }
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
        if refresh
            || !rels
                .iter()
                .all(|r| st.leases.iter().any(|l| l.covers_path(r)))
        {
            // 全件まとめて確保する (`try_claim` は全か無か)。
            let _ = try_claim(st, &holder, &rels, now, DEFAULT_TTL_SECS, alive);
        }
        Verdict::Allow
    });
    match outcome {
        Ok(Verdict::Deny(reason)) => {
            log_line(
                &dir,
                &format!("deny {} {}", holder.display(), rels.join(" ")),
            );
            deny_answer(agent, &reason)
        }
        Ok(Verdict::Allow) => pass_answer(),
        Err(e) if is_lock_busy(&e) => {
            // **混んでいるだけなら止める (fail-closed)。**
            // ここを通していたため、書き込みが多いときに「誰も持っていない」
            // と判定したまま登録に失敗し、**同じファイルへ複数のエージェントが
            // 入れた** (実測: 1500 書込 × 16 体で 42 ファイルが重複、うち 3 件は
            // 本物のマージ衝突になった)。いちばん混んでいる時 = いちばん
            // 衝突しやすい時にだけ効かなくなる、最悪の壊れ方だった。
            // 再試行すれば直るので、止めても作業は進む。
            log_line(&dir, &format!("busy-deny {}", rels.join(" ")));
            deny_answer(agent, &tr(
                "ファイル所有の台帳が混み合っていて、担当を登録できませんでした。\n                 そのまま書くと他の担当と同じファイルを触る恐れがあるため、いったん止めます。\n                 対処: 数秒おいて同じ操作をやり直してください (再試行すれば通ります)",
            ))
        }
        Err(e) => {
            // 台帳が壊れている / 読めない = こちらの都合。**ここは通す。**
            // エージェント全体の書き込みを台帳の破損で止めると、
            // ユーザーは機能ごと切る (切られた機能の保証はゼロ)。
            // エディタ自身の保存経路 (`check_write`) は fail-closed なので、
            // 手元の編集は守られる。
            log_line(&dir, &format!("fail-open {}: {e}", rels.join(" ")));
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
            Tier::Enforced => {
                // **「全部止まる」と読ませない。** 止まるのはフックを設置できた
                // エージェントの書き込みだけで、カタログにフックを持たない
                // エージェント (現状は claude 以外のほぼ全部) は対象外。
                // ここを大きく書くと「強制と出ているのに止まらない」になり、
                // このモジュール自身が「無いより悪い」と呼んでいる状態になる。
                "フックを設置したエージェントの書き込みはブロックされます (フックを持たないエージェントとエディタ外の書き込みは対象外)"
            }
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

/// **実際にブロックできるエージェント**の一覧 (カタログから起こす)。
///
/// 画面に「強制」とだけ出すと、対応していないエージェントを使っている人が
/// 「自分も守られている」と誤解する。誰が対象なのかを必ず名前で出すこと。
pub fn gated_agents() -> Vec<&'static str> {
    crate::agents::HOOK_TARGETS.iter().map(|t| t.bin).collect()
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
    settings: &[
        crate::feature::Setting {
            key: "lease.auto_arm",
            label: "worktree を見つけたらファイル所有ガードを自動で有効にする",
            help: "linked worktree がぶら下がっている / 自分がその中にいるときだけ有効になります。                   単独のリポジトリでは何もしません。",
            default: crate::feature::SettingValue::Bool(true),
        },
        crate::feature::Setting {
            key: "lease.ttl_minutes",
            label: "ファイル所有の寿命 (分)",
            help: "この時間だけ黙っている担当は所有権を失います。                   死んだエージェントにリポジトリを人質へ取らせないための上限です。",
            default: crate::feature::SettingValue::Int(30),
        },
    ],
    binds: &[],
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
pub fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

// ═══════════════════════════════════════════════════════════════════════════
//  8. 編集者ガード — 「このエディタを使っていれば衝突しない」の実体
// ═══════════════════════════════════════════════════════════════════════════
//
//  ここまでの節 (フック経路) が守るのは**外部 CLI エージェントの書き込み**だけ
//  で、**このエディタ自身の保存は素通り**していた。つまり 2 つの worktree で
//  Zaivern Code を開いて同じファイルを直接編集すると、台帳は 1 件も見ずに
//  両方が書けてしまう。それでは「このエディタを使っていれば衝突しない」とは
//  言えない。この節がその穴を塞ぐ。
//
//  ## 3 つの約束
//
//  1. **編集を始めた瞬間に確保する** (開いただけでは取らない)。読むだけの
//     タブが所有権を握ると、閲覧しているあいだ他人が永久に待たされる。
//  2. **UI スレッドを絶対に待たせない**。確保と解放はワーカースレッドで行い、
//     描画は「いま手元にある答え」(古くてよい) だけを見る。CLAUDE.md の
//     「git は UI スレッドで待たない」と同じ規律。
//  3. **保存の直前は fail-closed で確かめる**。ここだけは古い答えを使わない。
//     台帳は tmp → rename で置き換わるので、**ロック無しで読んでも
//     書きかけを見ることはない** — だから同期で読んでも数百マイクロ秒で返る。
//
//  ## なぜ「絶対」と言えるのか / 言えないのか
//
//  同じファイルを 2 人が同時に編集する状況そのものが起きなくなるので、
//  **テキストとしてのマージ衝突は構造的に起こらない**。一方で、別々の
//  ファイルを触った結果が意味的に噛み合わない (API を片方が変え、もう片方が
//  古い呼び方のまま等) ことは防げない。**そこは正直に区別する。**

/// 1 つのファイルについて、いま分かっている所有状態。
///
/// `Pending` の間も**編集は止めない**。確保の往復でキー入力が詰まるくらいなら、
/// answer が返った時点で知らせるほうがよい (保存は必ず [`check_write`] を通る
/// ので、取り損ねたまま書けてしまうことはない)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Own {
    /// 機能が無効、またはスコープ外。関知しない。
    Off,
    /// 確保を依頼したが答えがまだ。
    Pending,
    /// 自分のもの。書いてよい。
    Mine { until: u64 },
    /// 他人のもの。**保存を止める**。
    Taken { owner: String, reason: String },
}

/// 状態が変わったことの知らせ (トーストに出す)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// スコープ相対のパス。
    pub rel: String,
    pub own: Own,
}

/// ワーカーへの依頼。
enum Req {
    /// 起動直後に 1 度だけ: 段を「強制」まで引き上げる。
    Enforce,
    Claim(String),
    Release(String),
    ReleaseAll,
}

/// ガードの状態。**ウィンドウより長生きさせる** (設計原則 1) ため、
/// `ZaivernApp` のフィールドではなくモジュール側に置く。
struct GuardState {
    /// 有効か。無効なら全経路が `stat` すら踏まずに抜ける (設計原則 3)。
    armed: bool,
    roots: Roots,
    store: PathBuf,
    holder: Holder,
    /// スコープ相対パス → 所有状態。
    own: BTreeMap<String, Own>,
    /// 未読の知らせ。[`pump`] が回収する。
    notices: Vec<Notice>,
    /// いまの段。**「効いていると思わせて実は勧告」は無いより悪い**ので、
    /// 画面にはこの値をそのまま出す。
    tier: Tier,
    /// リースの寿命 (秒)。`lease.ttl_minutes` 由来。
    ttl: u64,
    tx: Option<Sender<Req>>,
}

impl Default for GuardState {
    fn default() -> Self {
        GuardState {
            armed: false,
            roots: Roots::default(),
            store: PathBuf::new(),
            holder: Holder::default(),
            own: BTreeMap::new(),
            notices: Vec::new(),
            tier: Tier::Off,
            ttl: DEFAULT_TTL_SECS,
            tx: None,
        }
    }
}

fn guard() -> &'static Mutex<GuardState> {
    static G: OnceLock<Mutex<GuardState>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(GuardState::default()))
}

/// このエディタ自身の持ち主。
///
/// **`session` を PID から起こすのが肝**で、これが無いと同じ worktree で
/// 2 つ起動したインスタンスが `cwd` + `agent` の一致で「同じ持ち主」に
/// 見えてしまい、互いの所有を素通りさせる ([`Holder::same`] の規則)。
fn editor_holder(tree: &Path) -> Holder {
    Holder {
        agent: tr("Zaivern Code"),
        session: format!("zai-{}", std::process::id()),
        cwd: normalize_path(&tree.to_string_lossy()),
        pid: std::process::id(),
    }
}

/// この場所に**衝突の危険があるか** (= 自動で有効化してよいか)。
///
/// 危険が無いのに常時有効化すると、単独で使っている人が台帳の読み書きを
/// 払わされる (設計原則 3: アイドル時のコストはゼロ)。逆に危険があるのに
/// 黙っていると「このエディタを使っていれば衝突しない」が嘘になる。
/// 判定は **stat 数回**で、git は 1 回も起動しない。
pub fn risky(roots: &Roots) -> bool {
    // 1. 自分が linked worktree にいる = 元リポジトリを誰かと分け合っている。
    if roots.key != roots.tree {
        return true;
    }
    // 2. 元リポジトリに linked worktree がぶら下がっている。
    let wt = roots.key.join(".git").join("worktrees");
    std::fs::read_dir(&wt).is_ok_and(|mut d| d.next().is_some())
}

/// ワークスペースを開いたときに 1 度だけ呼ぶ。**危険があれば自動で有効化する**。
///
/// 返り値は「このスコープでガードが効いているか」。
pub fn arm(start: &Path, auto: bool, ttl_minutes: i64) -> bool {
    arm_in(&store_dir(), start, auto, ttl_minutes)
}

/// 設定の分をリースの寿命 (秒) へ。極端な値でも壊れないように畳む。
fn ttl_from_minutes(minutes: i64) -> u64 {
    let m = minutes.clamp(1, 24 * 60) as u64;
    m.saturating_mul(60)
}

/// 台帳の置き場所を明示する [`arm`]。
///
/// 本番は [`store_dir`] (`~/.zaivern/leases`) を渡す。**テストが実 `~/.zaivern`
/// に触れないため**に置き場所を引数へ出してある (環境変数で分岐させると、
/// 本番の経路にテスト専用の枝が残る)。
pub fn arm_in(dir: &Path, start: &Path, auto: bool, ttl_minutes: i64) -> bool {
    let roots = roots_of(start);
    let store = store_path_in(dir, &roots.key);
    // 既に有効なら尊重する。無効でも危険があれば自動で有効化する
    // (`lease.auto_arm` を切っている人には勝手に入らない)。
    if !enabled(&store) && auto && risky(&roots) && enable(&store).is_err() {
        return false;
    }
    if !enabled(&store) {
        let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
        *g = GuardState::default();
        return false;
    }
    let holder = editor_holder(&roots.tree);
    let ttl = ttl_from_minutes(ttl_minutes);
    let roots_bg = roots.clone();
    let (tx, rx) = std::sync::mpsc::channel::<Req>();
    {
        let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
        g.armed = true;
        g.roots = roots;
        g.store = store.clone();
        g.holder = holder.clone();
        g.own.clear();
        g.notices.clear();
        g.tier = Tier::Advisory;
        g.ttl = ttl;
        g.tx = Some(tx.clone());
    }
    // 段の引き上げ (設定ファイルの読み書き) は**必ず裏で**。
    let _ = tx.send(Req::Enforce);
    // ワーカー。**I/O 中は状態のロックを握らない** (握ると描画が止まる)。
    std::thread::Builder::new()
        .name("zai-lease-guard".into())
        .spawn(move || {
            while let Ok(req) = rx.recv() {
                match req {
                    Req::Enforce => {
                        let t = ensure_enforced(&roots_bg);
                        let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
                        if g.store != store {
                            break;
                        }
                        g.tier = t;
                    }
                    Req::Claim(rel) => {
                        let own = claim_for(&store, &holder, &rel, now_secs(), ttl, &pid_alive);
                        let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
                        // arm し直された後の遅れた答えは捨てる。
                        if g.store != store {
                            break;
                        }
                        g.notices.push(Notice {
                            rel: rel.clone(),
                            own: own.clone(),
                        });
                        g.own.insert(rel, own);
                    }
                    Req::Release(rel) => {
                        let _ = release_one(&store, &holder, &rel);
                        let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
                        if g.store != store {
                            break;
                        }
                        g.own.remove(&rel);
                    }
                    Req::ReleaseAll => {
                        let _ = with_store(&store, |s| release(s, &holder));
                        let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
                        if g.store != store {
                            break;
                        }
                        g.own.clear();
                    }
                }
            }
        })
        .ok();
    true
}

/// 段を「強制」まで引き上げる。**ワーカースレッド専用** (設定ファイルを触る)。
///
/// 台帳を置くだけでは [`Tier::Advisory`] = 画面が警告するだけで、
/// **エージェントの書き込みは 1 件も止まらない**。「このエディタを使っていれば
/// 衝突しない」と言うためには、エージェント側のフックまで設置して
/// [`Tier::Enforced`] にする必要がある。
///
/// 設置は [`crate::supervisor::hooks::install`] が既存の設定を**バックアップしてから**
/// 行い、[`crate::supervisor::hooks::uninstall`] で元へ戻せる。だから自動でやってよい —
/// 戻せない変更なら聞くべきだが、これは戻せる。
fn ensure_enforced(roots: &Roots) -> Tier {
    let now = current_tier(roots);
    if now != Tier::Advisory {
        return now;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("zai"));
    for t in crate::agents::HOOK_TARGETS {
        let Some(plan) = crate::supervisor::hooks::plan_for(t.bin, &roots.tree, &exe) else {
            continue;
        };
        // 既に入っているものは触らない (ユーザーの設定を上書きしない)。
        if crate::supervisor::hooks::status(&plan)
            == crate::supervisor::hooks::HookStatus::Installed
        {
            continue;
        }
        let _ = crate::supervisor::hooks::install(&plan);
    }
    current_tier(roots)
}

/// いまの段 (**古くてよい**)。画面にそのまま出す。
pub fn tier_now() -> Tier {
    guard()
        .lock()
        .map(|g| if g.armed { g.tier } else { Tier::Off })
        .unwrap_or(Tier::Off)
}

/// ガードを降ろす (ワークスペースを閉じた / テストの後始末)。
///
/// ワーカーは送信端が落ちた時点で `recv` が失敗して自然に終わる。
#[cfg(test)]
pub fn disarm() {
    let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
    *g = GuardState::default();
}

/// ガードが効いているか。描画側の早期脱出用。
pub fn armed() -> bool {
    guard().lock().map(|g| g.armed).unwrap_or(false)
}

/// 絶対パスをスコープ相対へ。スコープ外なら `None` = 関知しない。
fn rel_of(g: &GuardState, abs: &Path) -> Option<String> {
    rel_within(&g.roots.tree, abs)
}

/// **いま編集中のファイル集合をまとめて渡す。これが唯一の同期口。**
///
/// 「始まった / 終わった」を対で呼ばせる形にすると、**対の片側を呼び忘れた
/// 経路が 1 つでもあると所有が漏れ続ける** (タブを閉じた・元に戻した・
/// ワークスペースを切り替えた…)。集合を丸ごと渡してもらい、
/// **消えたものはこちらで解放する**ほうが漏れようがない。
///
/// 渡すのは「パスがあって・汚れている」バッファだけ。開いただけのタブを
/// 入れないこと — 読むだけのタブが所有権を握ると、閲覧しているあいだ
/// 他人が待たされる。
pub fn sync_edits(paths: &[PathBuf]) {
    if !armed() {
        return;
    }
    let want: Vec<String> = {
        let g = guard().lock().unwrap_or_else(|e| e.into_inner());
        paths.iter().filter_map(|p| rel_of(&g, p)).collect()
    };
    for rel in &want {
        edit_begin_rel(rel);
    }
    let stale: Vec<String> = {
        let g = guard().lock().unwrap_or_else(|e| e.into_inner());
        g.own
            .keys()
            .filter(|k| !want.contains(k))
            .cloned()
            .collect()
    };
    for rel in stale {
        edit_end_rel(&rel);
    }
}

/// スコープ相対で確保を依頼する。冪等。
fn edit_begin_rel(rel: &str) {
    let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
    if !g.armed || g.own.contains_key(rel) {
        return;
    }
    g.own.insert(rel.to_string(), Own::Pending);
    if let Some(tx) = g.tx.as_ref() {
        let _ = tx.send(Req::Claim(rel.to_string()));
    }
}

/// スコープ相対で解放する。
fn edit_end_rel(rel: &str) {
    let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
    if !g.armed || g.own.remove(rel).is_none() {
        return;
    }
    if let Some(tx) = g.tx.as_ref() {
        let _ = tx.send(Req::Release(rel.to_string()));
    }
}

#[allow(dead_code)]
fn edit_begin(abs: &Path) {
    let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
    if !g.armed {
        return;
    }
    let Some(rel) = rel_of(&g, abs) else { return };
    // 既に確保済み / 依頼済みなら何もしない (キー入力ごとに依頼を積まない)。
    if g.own.contains_key(&rel) {
        return;
    }
    g.own.insert(rel.clone(), Own::Pending);
    if let Some(tx) = g.tx.as_ref() {
        let _ = tx.send(Req::Claim(rel));
    }
}

/// **もう編集しない** (タブを閉じた / 保存して汚れが消えた)。裏で解放する。
#[allow(dead_code)]
fn edit_end(abs: &Path) {
    let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
    if !g.armed {
        return;
    }
    let Some(rel) = rel_of(&g, abs) else { return };
    if g.own.remove(&rel).is_none() {
        return;
    }
    if let Some(tx) = g.tx.as_ref() {
        let _ = tx.send(Req::Release(rel));
    }
}

/// いま分かっている所有状態 (**古くてよい**)。描画から毎フレーム呼ぶ。
pub fn own_of(abs: &Path) -> Own {
    let g = guard().lock().unwrap_or_else(|e| e.into_inner());
    if !g.armed {
        return Own::Off;
    }
    let Some(rel) = rel_of(&g, abs) else {
        return Own::Off;
    };
    g.own.get(&rel).cloned().unwrap_or(Own::Off)
}

/// 背景の答えを回収する。毎フレーム呼び、返ったものをトーストに出す。
pub fn pump() -> Vec<Notice> {
    let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
    if g.notices.is_empty() {
        return Vec::new();
    }
    std::mem::take(&mut g.notices)
}

/// 終了時 / ワークスペースを閉じるときに自分の所有を全部返す。
///
/// **返し損ねても TTL で必ず回収される**が、返せば次の担当がすぐ入れる。
pub fn release_all() {
    let g = guard().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = g.tx.as_ref() {
        let _ = tx.send(Req::ReleaseAll);
    }
}

/// **保存の直前の最終確認。** ここが fail-closed の関門。
///
/// 台帳を**ロック無しで**読む: 置き換えは tmp → rename なので書きかけは
/// 見えず、ロック待ち (最大 [`LOCK_WAIT_MS`]) を UI スレッドへ持ち込まずに
/// 済む。台帳が読めない / 壊れているときは **fail-open** で通す —
/// 保存できないほうがユーザーの損害が大きいので、ここは安全側が「通す」。
pub fn check_write(abs: &Path) -> Verdict {
    let (store, holder, tree) = {
        let g = guard().lock().unwrap_or_else(|e| e.into_inner());
        if !g.armed {
            return Verdict::Allow;
        }
        (g.store.clone(), g.holder.clone(), g.roots.tree.clone())
    };
    let Some(rel) = rel_within(&tree, abs) else {
        return Verdict::Allow;
    };
    check_one(&store, &holder, &rel, now_secs(), &pid_alive)
}

/// `abs` **とその配下**に他人の所有があるか。フォルダの移動 / 削除の門。
///
/// [`check_write`] は 1 つのパスしか見ないので、**`src/` を消す操作は
/// 誰かが `src/app.rs` を確保していても素通りする**。フォルダごと動かす /
/// 捨てる操作は、中身の所有者にとっては上書きより強い破壊なので、
/// 配下まで見て止める。
///
/// ファイルを渡したときは [`check_write`] と同じ結果になる。
pub fn check_tree(abs: &Path) -> Verdict {
    let (store, holder, tree) = {
        let g = guard().lock().unwrap_or_else(|e| e.into_inner());
        if !g.armed {
            return Verdict::Allow;
        }
        (g.store.clone(), g.holder.clone(), g.roots.tree.clone())
    };
    let Some(rel) = rel_within(&tree, abs) else {
        return Verdict::Allow;
    };
    let Ok(st) = read_store(&store) else {
        // 読めないときの向きは [`check_write`] と揃える (そちらが唯一の規範)。
        return check_write(abs);
    };
    let now = now_secs();
    let alive: &dyn Fn(u32) -> bool = &pid_alive;
    // まず自分自身。次に配下。
    if let Verdict::Deny(m) = decide(&st, &holder, &rel, now, alive) {
        return Verdict::Deny(m);
    }
    let prefix = format!("{rel}/");
    for l in &st.leases {
        if l.holder.same(&holder) || !l.active(now, alive) {
            continue;
        }
        // 配下を指すパターンを 1 つでも持っていれば止める。
        if let Some(hit) = l.patterns.iter().find(|p| p.starts_with(&prefix)) {
            return Verdict::Deny(deny_reason(hit, l, now));
        }
    }
    Verdict::Allow
}

// ── 単体で試せる中身 (シングルトンを経由しない) ────────────────────────────

/// 1 パスを確保して、結果を [`Own`] で返す。
#[cfg(test)]
pub fn claim_one(
    store: &Path,
    holder: &Holder,
    rel: &str,
    now: u64,
    alive: &dyn Fn(u32) -> bool,
) -> Own {
    claim_for(store, holder, rel, now, DEFAULT_TTL_SECS, alive)
}

/// 寿命を明示する確保。
pub fn claim_for(
    store: &Path,
    holder: &Holder,
    rel: &str,
    now: u64,
    ttl: u64,
    alive: &dyn Fn(u32) -> bool,
) -> Own {
    let pats = vec![rel.to_string()];
    match with_store(store, |s| try_claim(s, holder, &pats, now, ttl, alive)) {
        // 台帳を触れないときは fail-open (編集を止めない)。保存前に再度見る。
        Err(_) => Own::Off,
        Ok(Claim::Granted(_)) => Own::Mine {
            until: now.saturating_add(ttl),
        },
        Ok(Claim::Refused { owner, .. }) => {
            let reason = match read_store(store) {
                Ok(s) => match decide(&s, holder, rel, now, alive) {
                    Verdict::Deny(m) => m,
                    Verdict::Allow => String::new(),
                },
                Err(_) => String::new(),
            };
            Own::Taken { owner, reason }
        }
    }
}

/// 1 パスを解放する。
pub fn release_one(store: &Path, holder: &Holder, rel: &str) -> Result<(), String> {
    with_store(store, |s| {
        for l in s.leases.iter_mut() {
            if l.holder.same(holder) {
                l.patterns.retain(|p| p != rel);
            }
        }
        s.leases.retain(|l| !l.patterns.is_empty());
    })
}

/// 保存直前の判定 (ロック無しで読む)。
pub fn check_one(
    store: &Path,
    holder: &Holder,
    rel: &str,
    now: u64,
    alive: &dyn Fn(u32) -> bool,
) -> Verdict {
    match read_store(store) {
        // **台帳が「在るのに読めない」ときは止める。**
        // ここを許可に倒していたため、台帳を壊す / 権限を落とすだけで
        // 誰でもガードを丸ごと無効化できた (敵対的検証で 7 通り破られた)。
        // 台帳が**無い**ときは `read_store` が空を返す = 機能が無効なので、
        // この腕に来るのは「在るのに読めない」ときだけ。
        // 文面には必ず**戻し方**を書く (でないとユーザーは機能を切るだけ)。
        Err(e) => Verdict::Deny(trf(
            "ファイル所有の台帳を読めないため、安全のため保存を止めました ({err})。\n             対処: (1) 台帳の権限を直す (2) `zai lease list` で状態を見る              (3) このワークスペースでガードを切る (`zai lease disable`)",
            &[("err", e)],
        )),
        Ok(s) => decide(&s, holder, rel, now, alive),
    }
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
        // **末尾の `/` は「配下ぜんぶ」。** ここを `src` に潰していたため
        // `zai lease claim src/` が 1 件も守らないのに成功を返していた。
        assert_eq!(normalize_path("src/"), "src/**");
        assert_eq!(normalize_path("src/ui/"), "src/ui/**");
        assert_eq!(normalize_path("/"), "");
        // `..` は畳む (確保側と書き込み側で形がずれないように)。
        assert_eq!(normalize_path("src/sub/../mod.rs"), "src/mod.rs");
        assert_eq!(normalize_path("a/b/../../c.rs"), "c.rs");
        // スコープの外へ出る `..` は落とす (外は関知しない)。
        assert_eq!(normalize_path("../../etc/passwd"), "etc/passwd");
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
        // 大小非区別のファイルシステムが既定の OS (Windows / macOS) では畳む。
        // Linux は畳まない — **両側を書く**。
        // macOS を入れていなかったため、開発機で `Foo.rs` と `foo.rs` が
        // 別リースになり同じ物理ファイルへ 2 人が書けていた。
        if cfg!(any(windows, target_os = "macos")) {
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
        let got = main_repo_root_from_pointer(
            "gitdir: /repos/proj/.git/worktrees/feat-a\n",
            Path::new("/wt"),
        );
        assert_eq!(got, Some(PathBuf::from("/repos/proj")));
        // Windows 形式
        let got = main_repo_root_from_pointer("gitdir: C:/r/p/.git/worktrees/w1", Path::new("/wt"));
        assert_eq!(got, Some(PathBuf::from("C:/r/p")));
        // 形が違えば推測しない
        assert_eq!(
            main_repo_root_from_pointer("gitdir: /repos/proj/.git", Path::new("/wt")),
            None
        );
        assert_eq!(
            main_repo_root_from_pointer("これは違う", Path::new("/wt")),
            None
        );
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
        let a = deny_answer("claude", "だめ");
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

#[cfg(test)]
mod guard_tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = crate::test_util::unique_temp_dir("lease-guard", tag);
        std::fs::create_dir_all(&d).expect("一時フォルダを作れない");
        d
    }

    /// 生きている扱い / 死んでいる扱いの偽 PID 判定。
    fn alive_all(_: u32) -> bool {
        true
    }
    fn alive_none(_: u32) -> bool {
        false
    }

    fn holder(name: &str, cwd: &str) -> Holder {
        Holder {
            agent: name.to_string(),
            // **worktree ごとに別セッション**にする。ここを同じにすると
            // `Holder::same` が「同じ持ち主」と見なして素通りする。
            session: format!("sess-{name}"),
            cwd: normalize_path(cwd),
            pid: 4242,
        }
    }

    /// **この機能の心臓**: 2 つの worktree が同じファイルを編集しようとしたら、
    /// 2 人目は所有を取れない。
    #[test]
    fn 別のworktreeが同じファイルを編集しようとすると所有が取れない() {
        let dir = tmp("two-trees");
        let store = dir.join("store.json");
        let a = holder("A", "/repo/.wt/a");
        let b = holder("B", "/repo/.wt/b");
        let now = 1_000;

        let got_a = claim_one(&store, &a, "src/app.rs", now, &alive_all);
        assert!(
            matches!(got_a, Own::Mine { .. }),
            "先に来た A が取れていない: {got_a:?}"
        );

        let got_b = claim_one(&store, &b, "src/app.rs", now, &alive_all);
        match got_b {
            Own::Taken { owner, reason } => {
                assert!(owner.contains('A'), "持ち主の名前が出ていない: {owner}");
                assert!(
                    reason.contains("src/app.rs"),
                    "拒否理由にパスが出ていない: {reason}"
                );
            }
            other => panic!("B が取れてしまった (衝突が起きる): {other:?}"),
        }
    }

    /// 別々のファイルなら 2 人とも取れる (過剰に締めない)。
    #[test]
    fn 別のファイルなら二人とも所有を取れる() {
        let dir = tmp("disjoint");
        let store = dir.join("store.json");
        let a = holder("A", "/repo/.wt/a");
        let b = holder("B", "/repo/.wt/b");
        let now = 1_000;
        assert!(matches!(
            claim_one(&store, &a, "src/app.rs", now, &alive_all),
            Own::Mine { .. }
        ));
        assert!(matches!(
            claim_one(&store, &b, "src/config.rs", now, &alive_all),
            Own::Mine { .. }
        ));
    }

    /// **保存直前の門は fail-closed。** 他人が持っていたら書かせない。
    #[test]
    fn 保存直前の判定は他人の所有を拒否する() {
        let dir = tmp("check-deny");
        let store = dir.join("store.json");
        let a = holder("A", "/repo/.wt/a");
        let b = holder("B", "/repo/.wt/b");
        let now = 1_000;
        claim_one(&store, &a, "src/app.rs", now, &alive_all);
        match check_one(&store, &b, "src/app.rs", now, &alive_all) {
            Verdict::Deny(m) => assert!(m.contains("src/app.rs"), "理由が薄い: {m}"),
            Verdict::Allow => panic!("他人のファイルへの保存が通ってしまった"),
        }
        // 持ち主自身は当然通る。
        assert_eq!(
            check_one(&store, &a, "src/app.rs", now, &alive_all),
            Verdict::Allow
        );
    }

    /// **台帳が「在るのに読めない」ときは止める (fail-closed)。**
    ///
    /// 以前はここを許可に倒していたが、それは**台帳を壊すだけで誰でも
    /// ガードを丸ごと無効化できる**ということだった (敵対的検証で 7 通り
    /// 破られた)。判断材料が無いなら書かせない。ただし**戻し方を文面に
    /// 必ず書く** — 出口の無い拒否は、ユーザーが機能を切って終わる。
    #[test]
    fn 読めない台帳では保存を止めて戻し方を示す() {
        let dir = tmp("broken");
        let store = dir.join("store.json");
        std::fs::write(&store, "これは JSON ではない {{{").expect("write");
        let a = holder("A", "/repo/.wt/a");
        match check_one(&store, &a, "src/app.rs", 1_000, &alive_all) {
            Verdict::Deny(m) => {
                assert!(
                    m.contains("lease"),
                    "戻し方 (コマンド) が書かれていない: {m}"
                );
            }
            Verdict::Allow => panic!("読めない台帳で保存が通ってしまった"),
        }
    }

    /// **台帳が無いだけなら止めない。** 「無効」と「壊れている」は別物で、
    /// ここを混ぜると使っていない人の保存まで止まる。
    #[test]
    fn 台帳が無いだけなら保存は止めない() {
        let dir = tmp("absent");
        let store = dir.join("store.json");
        let a = holder("A", "/repo/.wt/a");
        assert_eq!(
            check_one(&store, &a, "src/app.rs", 1_000, &alive_all),
            Verdict::Allow,
            "台帳が無いだけで保存を止めてはいけない"
        );
    }

    /// 解放したら次の担当がすぐ取れる (待たせた時間がそのまま損害になる)。
    #[test]
    fn 解放すると次の担当が取れる() {
        let dir = tmp("handover");
        let store = dir.join("store.json");
        let a = holder("A", "/repo/.wt/a");
        let b = holder("B", "/repo/.wt/b");
        let now = 1_000;
        claim_one(&store, &a, "src/app.rs", now, &alive_all);
        release_one(&store, &a, "src/app.rs").expect("解放できない");
        assert!(
            matches!(
                claim_one(&store, &b, "src/app.rs", now, &alive_all),
                Own::Mine { .. }
            ),
            "解放したのに次が取れない"
        );
    }

    /// 死んだ担当のリースは期限で回収される (リポジトリを人質に取らせない)。
    #[test]
    fn 期限切れで死んだ担当の所有は回収される() {
        let dir = tmp("expire");
        let store = dir.join("store.json");
        let a = holder("A", "/repo/.wt/a");
        let b = holder("B", "/repo/.wt/b");
        let now = 1_000;
        claim_one(&store, &a, "src/app.rs", now, &alive_all);
        // TTL を過ぎ、かつ持ち主のプロセスも死んでいる。
        let later = now + DEFAULT_TTL_SECS + 1;
        assert!(
            matches!(
                claim_one(&store, &b, "src/app.rs", later, &alive_none),
                Own::Mine { .. }
            ),
            "期限切れ + プロセス死亡でも回収されない"
        );
    }

    /// glob で受け持った担当は、その配下の具体ファイルも守る。
    #[test]
    fn globで受け持つと配下の具体ファイルも守られる() {
        let dir = tmp("glob");
        let store = dir.join("store.json");
        let a = holder("A", "/repo/.wt/a");
        let b = holder("B", "/repo/.wt/b");
        let now = 1_000;
        claim_one(&store, &a, "src/ui/**", now, &alive_all);
        assert!(
            matches!(
                check_one(&store, &b, "src/ui/panel.rs", now, &alive_all),
                Verdict::Deny(_)
            ),
            "glob の配下が守られていない"
        );
    }

    /// **危険がないときは自動で有効化しない** (単独利用者に払わせない)。
    #[test]
    fn worktreeが無いリポジトリでは自動で有効化しない() {
        let dir = tmp("solo");
        std::fs::create_dir_all(dir.join(".git")).expect("mkdir .git");
        let roots = roots_of(&dir);
        assert!(!risky(&roots), "単独リポジトリを危険と判定している");
    }

    /// worktree がぶら下がっていれば危険 = 自動で有効化してよい。
    #[test]
    fn worktreeがぶら下がっていれば危険と判定する() {
        let dir = tmp("has-wt");
        std::fs::create_dir_all(dir.join(".git").join("worktrees").join("w1"))
            .expect("mkdir worktrees");
        let roots = roots_of(&dir);
        assert!(risky(&roots), "worktree があるのに危険と判定していない");
    }

    /// 自分が linked worktree の中にいれば危険。
    ///
    /// **`key` と `tree` が別物になる**のがこの状況の目印で、ここを
    /// 取り違えると機能が丸ごと無言で効かなくなる。
    #[test]
    fn linked_worktreeの中にいれば危険と判定する() {
        let base = tmp("linked");
        let main = base.join("main");
        std::fs::create_dir_all(main.join(".git").join("worktrees").join("w1")).expect("mkdir");
        let wt = base.join("w1");
        std::fs::create_dir_all(&wt).expect("mkdir wt");
        // linked worktree の `.git` は元リポジトリを指すファイル。
        std::fs::write(
            wt.join(".git"),
            format!(
                "gitdir: {}\n",
                main.join(".git").join("worktrees").join("w1").display()
            ),
        )
        .expect("write pointer");
        let roots = roots_of(&wt);
        assert_ne!(roots.key, roots.tree, "key と tree が分かれていない");
        assert!(risky(&roots), "linked worktree を危険と判定していない");
    }
}

/// **端から端まで**の検査 — エディタの保存経路が本当に止まるか。
///
/// ここだけはシングルトン (`guard()`) を触るので、`mod guard_tests` の
/// 並列実行と干渉しないよう**このモジュール内で直列化**し、後始末で必ず
/// [`disarm`] する。実 `~/.zaivern` には触れない ([`arm_in`] に一時フォルダを渡す)。
#[cfg(test)]
mod guard_e2e_tests {
    use super::*;

    /// シングルトンを触るテストどうしを直列化する。
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// linked worktree を 1 本持つリポジトリを作り、その worktree の場所を返す。
    fn repo_with_worktree(tag: &str) -> (PathBuf, PathBuf) {
        let base = crate::test_util::unique_temp_dir("lease-e2e", tag);
        let main = base.join("main");
        std::fs::create_dir_all(main.join(".git").join("worktrees").join("w1"))
            .expect("mkdir main");
        let wt = base.join("w1");
        std::fs::create_dir_all(wt.join("src")).expect("mkdir wt");
        std::fs::write(
            wt.join(".git"),
            format!(
                "gitdir: {}\n",
                main.join(".git").join("worktrees").join("w1").display()
            ),
        )
        .expect("write pointer");
        (base, wt)
    }

    /// **この機能の本番の主張**: 別の担当が持っているファイルは、
    /// エディタの保存直前の門で止まる。
    #[test]
    fn 他人が持つファイルはエディタの保存門で止まる() {
        let _s = serial();
        let (base, wt) = repo_with_worktree("deny");
        let dir = base.join("ledger");

        // worktree がぶら下がっているので、開いた時点で自動で有効になる。
        assert!(
            arm_in(&dir, &wt, true, 30),
            "worktree があるのに有効化されない"
        );

        // 別の担当 (別ワークツリーのエディタ) が先に確保する。
        let roots = roots_of(&wt);
        let store = store_path_in(&dir, &roots.key);
        let other = Holder {
            agent: "別の担当".into(),
            session: "sess-other".into(),
            cwd: normalize_path("/somewhere/else"),
            pid: 4242,
        };
        assert!(matches!(
            claim_one(&store, &other, "src/app.rs", now_secs(), &|_| true),
            Own::Mine { .. }
        ));

        // エディタが保存しようとする → 止まる。
        let target = wt.join("src").join("app.rs");
        match check_write(&target) {
            Verdict::Deny(m) => {
                assert!(m.contains("別の担当"), "誰が持っているか出ていない: {m}");
                assert!(m.contains("src/app.rs"), "どのファイルか出ていない: {m}");
            }
            Verdict::Allow => panic!("他人が持つファイルへの保存が通ってしまった"),
        }

        // 自分が持っているファイルは当然通る。
        let mine = wt.join("src").join("mine.rs");
        assert_eq!(check_write(&mine), Verdict::Allow, "空きファイルが通らない");

        disarm();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// **`lease.auto_arm` を切っている人には勝手に入らない。**
    /// 設定が効かないなら、その設定は無いほうがよい。
    #[test]
    fn 自動有効化を切っていればworktreeがあっても有効にならない() {
        let _s = serial();
        let (base, wt) = repo_with_worktree("no-auto");
        let dir = base.join("ledger");
        assert!(
            !arm_in(&dir, &wt, false, 30),
            "auto_arm を切っているのに有効化された"
        );
        assert!(!armed());
        assert_eq!(
            check_write(&wt.join("src").join("app.rs")),
            Verdict::Allow,
            "切っているのに判断している"
        );
        disarm();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 寿命の設定が実際にリースへ載る (極端な値でも壊れない)。
    #[test]
    fn 寿命の設定は畳まれてリースに載る() {
        assert_eq!(ttl_from_minutes(30), 30 * 60);
        // 0 や負数でも「即失効」にはしない (取った瞬間に他人へ奪われる)。
        assert_eq!(ttl_from_minutes(0), 60);
        assert_eq!(ttl_from_minutes(-5), 60);
        // 上限は 24 時間。放置された担当がリポジトリを永久に握らない。
        assert_eq!(ttl_from_minutes(i64::MAX), 24 * 60 * 60);
    }

    /// **フォルダごとの移動 / 削除は、配下の所有者に阻まれる。**
    ///
    /// `check_write` は 1 パスしか見ないので、`src/` を消す操作は
    /// `src/app.rs` の持ち主がいても素通りしていた。消すのは戻せないので、
    /// 上書きより強く守る必要がある。
    #[test]
    fn フォルダの削除は配下の所有者に阻まれる() {
        let _s = serial();
        let (base, wt) = repo_with_worktree("tree");
        let dir = base.join("ledger");
        assert!(arm_in(&dir, &wt, true, 30));

        let roots = roots_of(&wt);
        let store = store_path_in(&dir, &roots.key);
        let other = Holder {
            agent: "別の担当".into(),
            session: "sess-other".into(),
            cwd: normalize_path("/somewhere/else"),
            pid: 4242,
        };
        claim_one(&store, &other, "src/app.rs", now_secs(), &|_| true);

        // 配下を持たれているフォルダは動かせない / 消せない。
        match check_tree(&wt.join("src")) {
            Verdict::Deny(m) => assert!(m.contains("別の担当"), "持ち主が出ていない: {m}"),
            Verdict::Allow => panic!("配下に所有があるフォルダの操作が通ってしまった"),
        }
        // 無関係なフォルダは通る (過剰に止めない)。
        assert_eq!(check_tree(&wt.join("docs")), Verdict::Allow);
        // ファイル単体なら check_write と同じ結果。
        assert!(matches!(
            check_tree(&wt.join("src").join("app.rs")),
            Verdict::Deny(_)
        ));

        disarm();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 自分が持っているフォルダは自分で動かせる (自分に阻まれない)。
    #[test]
    fn 自分が持つフォルダは自分で動かせる() {
        let _s = serial();
        let (base, wt) = repo_with_worktree("tree-mine");
        let dir = base.join("ledger");
        assert!(arm_in(&dir, &wt, true, 30));
        // ガードが握っている持ち主そのもので確保する。
        let roots = roots_of(&wt);
        let store = store_path_in(&dir, &roots.key);
        let me = editor_holder(&roots.tree);
        claim_one(&store, &me, "src/app.rs", now_secs(), &|_| true);
        assert_eq!(check_tree(&wt.join("src")), Verdict::Allow);
        disarm();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// ガードを降ろしたら 1 件も判断しない (単独利用者のコストはゼロ)。
    #[test]
    fn 降ろした後は何も判断しない() {
        let _s = serial();
        let (base, wt) = repo_with_worktree("off");
        let dir = base.join("ledger");
        arm_in(&dir, &wt, true, 30);
        disarm();
        assert!(!armed());
        assert_eq!(
            check_write(&wt.join("src").join("app.rs")),
            Verdict::Allow,
            "降ろしたのに判断している"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod scale_regression_tests {
    use super::*;

    /// **部分木の確保が実際に配下を守る。**
    /// 人が担当表を書くときに最も自然な `src/` が no-op だった
    /// (しかも「確保しました」と返っていた) ので、番人を置く。
    #[test]
    fn 末尾スラッシュの確保は配下を守る() {
        let pat = normalize_path("src/");
        assert!(covers(&pat, &normalize_path("src/app.rs")));
        assert!(covers(&pat, &normalize_path("src/ui/panel.rs")));
        assert!(!covers(&pat, &normalize_path("tests/app.rs")));
        // 事前の重なり検出でも同じ形で効く。
        assert!(overlaps(&pat, &normalize_path("src/app.rs")));
    }

    /// `..` を含む指定でも、実際に書かれるパスと一致する。
    #[test]
    fn 相対のドット二つを含む確保も実パスに当たる() {
        let pat = normalize_path("src/sub/../mod.rs");
        assert_eq!(pat, normalize_path("src/mod.rs"));
        assert!(covers(&pat, &normalize_path("src/mod.rs")));
    }

    /// **`**` が並んでも判定が爆発しない。**
    /// 素の再帰では `**` 8 個で 1 件の判定に 35 秒かかっていた
    /// (= 書き込みの臨界路が丸ごと止まる)。
    #[test]
    fn ワイルドカードが多段でも判定が爆発しない() {
        let pat = "**/**/**/**/**/**/**/**/x.rs";
        let path = "a/b/c/d/e/f/g/h/i/j/k/l/y.rs";
        let t0 = Instant::now();
        assert!(!covers(pat, path), "当たらないはず");
        let dt = t0.elapsed();
        assert!(
            dt < Duration::from_millis(200),
            "多段ワイルドカードで爆発している: {dt:?}"
        );
    }

    /// 混み合って登録できなかったことを、文面ではなく印で見分ける。
    #[test]
    fn 混雑と破損を区別できる() {
        assert!(is_lock_busy(&format!("{LOCK_BUSY}混んでいます")));
        assert!(!is_lock_busy("台帳が壊れています: expected value"));
    }
}

#[cfg(test)]
mod os_rule_tests {
    use super::*;

    /// **3 つの OS の規則を、どのホストからでも固定する。**
    ///
    /// `cfg!` 分岐のままだと、macOS で開発している限り Windows / Linux の
    /// 規則は一度も実行されない。実際に「macOS だけ大小を畳んでいなかった」
    /// ために、開発機で `Foo.rs` と `foo.rs` が別リースになっていた。
    #[test]
    fn 三つのosの正規化規則を表で固定する() {
        // (win_sep, fold_case, 入力, 期待)
        let table: &[(bool, bool, &str, &str)] = &[
            // Windows: 区切りを畳み、大小も畳む
            (true, true, "SRC\\App.rs", "src/app.rs"),
            (true, true, "src\\ui\\", "src/ui/**"),
            // macOS: 同上 (APFS は大小非区別)
            (true, true, "SRC/App.rs", "src/app.rs"),
            // Linux: 大小は畳まない
            (true, false, "SRC/App.rs", "SRC/App.rs"),
            (true, false, "src/ui/", "src/ui/**"),
            // 区切りを畳まない設定 (参考: unix の `\` を名前の一部として扱う)
            (false, false, "src/a\\b.rs", "src/a\\b.rs"),
            // `..` の畳み込みは規則に依らない
            (true, false, "src/sub/../mod.rs", "src/mod.rs"),
            (true, true, "src/sub/../MOD.rs", "src/mod.rs"),
        ];
        for (win_sep, fold, input, want) in table {
            assert_eq!(
                normalize_path_on(input, *win_sep, *fold),
                *want,
                "win_sep={win_sep} fold_case={fold} 入力={input:?}"
            );
        }
    }

    /// 既定は動いている OS の規則を選ぶ (公開シグネチャは据え置き)。
    #[test]
    fn 既定の規則は動いているosに一致する() {
        let want = normalize_path_on("SRC/App.rs", true, cfg!(any(windows, target_os = "macos")));
        assert_eq!(normalize_path("SRC/App.rs"), want);
    }
}
