//! エージェント別 worktree 隔離と、ファイル衝突の事前検出 (共有基盤)。
//!
//! ここは **`race.rs` の中に閉じ込められていた部品を外へ出した層**である。
//! レース (`race.rs`) と通常の Cockpit 並列作業の両方が同じ関数を通るので、
//! 「レースのときだけ衝突が分かる」という非対称が無くなる。
//!
//! ## 提供するもの
//!
//! 1. **git worktree の作成 / 削除** — [`create_agent_worktree`] /
//!    [`remove_agent_worktree`]。命名は純粋関数 ([`agent_branch`] /
//!    [`agent_dir_name`] / [`pick_agent_slot`]) に切り出してあり、
//!    ユーザー名・ホームパス・ロケールに一切依存しない。
//! 2. **触ったファイルの走査** — [`scan_touched`] (`status --porcelain=v1 -z` と
//!    `diff --name-only -z`)。`-z` なので空白・日本語・改行入りのパスでも壊れない。
//!    git はロケール依存の文言を読まないよう `LC_ALL=C` で固定して呼ぶ。
//! 3. **衝突の畳み込み** — [`compute_overlaps`] (添字ベース・レース用) と
//!    [`build_conflicts`] (セッション ID ベース・Cockpit 用)。
//! 4. **監視** — [`ConflictWatch`]。同じ作業ツリーに **2 体以上** が居るときだけ
//!    走査を起こす。1 体以下なら git を 1 回も叩かない (アイドルのコストはゼロ)。
//!
//! ## 「誰が触ったか」をどう決めているか (正直な線引き)
//!
//! 同じ作業ツリーを共有している複数エージェントの変更は、git からは
//! **区別できない** (同じ index / 同じ working tree なので)。画面から推測するのは
//! 設計原則 4 が禁じている。そこでこの層は推測をやめ、
//! **「同じツリーに同居している稼働中のエージェント全員が、そのツリーの
//! 変更ファイルを取り合っている」** という事実だけを報告する。
//! 誤検出 (実際には 1 体しか触っていない) はあり得るが、**見落としは無い**。
//! 並列作業で本当に困るのは見落としの方なので、この向きに倒す。
//!
//! worktree で隔離したエージェントは cwd が別なので、たとえ両方が
//! `src/app.rs` を編集していても衝突として数えない ([`build_conflicts`] は
//! cwd を結合した絶対パスを鍵にする)。これが隔離の効き目そのものである。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::i18n::{tr, trf};

/// ブランチ名に使うスラグの最大長 (ASCII 前提)。
pub const SLUG_MAX: usize = 24;

/// 同居エージェントの走査間隔。race の差分ポーリング (4 秒) と同じ桁。
const SCAN_TTL: Duration = Duration::from_secs(6);

// ---------------------------------------------------------------------------
// git 実行 (すべて明示パス・LC_ALL=C)
// ---------------------------------------------------------------------------

/// `git -C <repo> <args...>` を窓なしで実行する。
///
/// 出力文言を判定に使う箇所があるので `LC_ALL=C` で英語に固定する
/// (ロケール依存の日本語メッセージをパースしないための約束)。
/// PATH の面倒は `procx` が見る。
pub fn git_out(repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = crate::procx::hidden_command("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| trf("git を起動できません: {e}", &[("e", e.to_string())]))?;
    let stdout = crate::textenc::decode_output(&out.stdout);
    if out.status.success() {
        Ok(stdout.trim_end().to_string())
    } else {
        let stderr = crate::textenc::decode_output(&out.stderr);
        let msg = if stderr.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        Err(format!(
            "git {}: {msg}",
            args.first().copied().unwrap_or_default()
        ))
    }
}

// ---------------------------------------------------------------------------
// 純粋ロジック: スラグ / 命名 / 置き場
// ---------------------------------------------------------------------------

/// 任意の文字列からブランチ用スラグを作る。ASCII 英数字だけを残し、
/// 他 (空白・`/`・日本語・絵文字) は `-` に潰して連結する。
/// 何も残らなければ `fallback`。
///
/// フォルダ名をそのままブランチ名にすると、`/` がネームスペース区切りに
/// 化けたり空白で `git` の引数が割れたりする。ここで必ず 1 度潰す。
pub fn slugify(text: &str, fallback: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = true; // 先頭のダッシュを抑止
    for c in text.chars() {
        if out.len() >= SLUG_MAX {
            break;
        }
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        fallback.to_string()
    } else {
        out
    }
}

/// worktree の置き場。基本はリポジトリの隣 (親フォルダ)。
/// リポジトリが `.claude/worktrees` 配下 (Claude セッションの管理領域) にある、
/// または親が取れない場合は `temp_dir()/<temp_leaf>` へ退避する。
///
/// **絶対にホームやユーザー名を組み立てない** — `std::env::temp_dir()` だけを使う。
pub fn worktree_base(repo: &Path, temp_leaf: &str) -> PathBuf {
    let inside_claude_worktrees = repo.ancestors().any(|a| {
        a.file_name().is_some_and(|n| n == "worktrees")
            && a.parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n == ".claude")
    });
    match repo.parent() {
        Some(p) if !inside_claude_worktrees && p != Path::new("") => p.to_path_buf(),
        _ => std::env::temp_dir().join(temp_leaf),
    }
}

/// `git worktree remove` の引数列。`--force` は確認を経た時だけ付く。
pub fn worktree_remove_args(dir: &str, force: bool) -> Vec<String> {
    let mut args = vec!["worktree".to_string(), "remove".to_string()];
    if force {
        args.push("--force".to_string());
    }
    args.push(dir.to_string());
    args
}

/// 隔離エージェント `n` (1 始まり) のブランチ名。
pub fn agent_branch(slug: &str, n: usize) -> String {
    format!("agent/{slug}-{n}")
}

/// 隔離エージェント `n` の worktree フォルダ名 (リポジトリの隣に置く)。
pub fn agent_dir_name(repo_name: &str, slug: &str, n: usize) -> String {
    format!("{repo_name}-agent-{slug}-{n}")
}

/// 空いている `(ブランチ名, フォルダ名)` を選ぶ純粋関数。
///
/// `taken(branch, dir_name)` が true の間だけ連番を進める。ブランチとフォルダを
/// **両方** 見るのが要点で、片方だけ残っている中途半端な状態 (worktree を手で
/// 消したがブランチが残っている等) でも必ず衝突しない名前に着地する。
pub fn pick_agent_slot(
    slug: &str,
    repo_name: &str,
    mut taken: impl FnMut(&str, &str) -> bool,
) -> (String, String) {
    let mut n = 1usize;
    loop {
        let branch = agent_branch(slug, n);
        let dir = agent_dir_name(repo_name, slug, n);
        if !taken(&branch, &dir) {
            return (branch, dir);
        }
        n += 1;
    }
}

/// `path` かその祖先に `.git` があるか。
///
/// **サブプロセスを起こさない**安価な判定で、メニューの可否表示 (毎フレーム
/// 描かれ得る場所) からはこちらを使う。実際に作るときは [`repo_root`] が
/// git 本体に問い合わせる。worktree の中では `.git` はファイルなので
/// `is_dir()` ではなく `exists()` で見る。
pub fn looks_like_git_repo(path: &Path) -> bool {
    path.ancestors().any(|a| a.join(".git").exists())
}

/// `path` を含む git リポジトリのルート (worktree の中なら worktree のルート)。
pub fn repo_root(path: &Path) -> Result<PathBuf, String> {
    git_out(path, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .map_err(|e| trf("git リポジトリではありません: {e}", &[("e", e)]))
}

// ---------------------------------------------------------------------------
// worktree の作成 / 削除
// ---------------------------------------------------------------------------

/// エージェント 1 体に割り当てた worktree。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentWorktree {
    /// 本体リポジトリ (`git -C` の相手)。
    pub repo: PathBuf,
    /// 切ったブランチ (`agent/<slug>-<n>`)。
    pub branch: String,
    /// worktree のフォルダ (= エージェントの cwd)。
    pub dir: PathBuf,
}

/// `root` を含むリポジトリに、エージェント専用の worktree を切る。
///
/// - ベースは現在の `HEAD` コミット。レースと違い**作業ツリーが汚れていても許す**
///   (隔離して走らせるだけで、後でマージを強制するわけではないため)。
/// - ブランチ名は `agent/<label のスラグ>-<連番>`。既存のブランチ / フォルダとは
///   [`pick_agent_slot`] が必ず衝突を避ける。
pub fn create_agent_worktree(root: &Path, label: &str) -> Result<AgentWorktree, String> {
    let repo = repo_root(root)?;
    let base_commit = git_out(&repo, &["rev-parse", "HEAD"]).map_err(|e| {
        trf(
            "コミットが 1 つも無いリポジトリでは worktree を作れません: {e}",
            &[("e", e)],
        )
    })?;
    let branches: HashSet<String> = git_out(
        &repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )?
    .lines()
    .map(str::to_string)
    .collect();
    let repo_name = repo
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let repo_slug = slugify(&repo_name, "repo");
    let base_dir = worktree_base(&repo, "zaivern-agents");
    let slug = slugify(label, &repo_slug);
    let (branch, dir_name) = pick_agent_slot(&slug, &repo_slug, |b, d| {
        branches.contains(b) || base_dir.join(d).exists()
    });
    std::fs::create_dir_all(&base_dir).map_err(|e| {
        trf(
            "worktree の置き場を作れません: {e}",
            &[("e", e.to_string())],
        )
    })?;
    let dir = base_dir.join(&dir_name);
    let dir_s = dir.to_string_lossy().into_owned();
    git_out(
        &repo,
        &["worktree", "add", "-b", &branch, &dir_s, &base_commit],
    )
    .map_err(|e| {
        trf(
            "worktree を作成できません ({branch}): {e}",
            &[("branch", branch.clone()), ("e", e)],
        )
    })?;
    Ok(AgentWorktree { repo, branch, dir })
}

/// worktree に未コミットの変更 (未追跡ファイルを含む) が残っているか。
///
/// 判定できないとき (フォルダが消えている等) は **true 側へ倒す** —
/// 「分からないなら消さないで確認する」方が安全なため。
pub fn worktree_is_dirty(dir: &Path) -> bool {
    match git_out(dir, &["status", "--porcelain"]) {
        Ok(s) => !s.trim().is_empty(),
        Err(_) => true,
    }
}

/// worktree を外し、ブランチも消す。
///
/// `force` が false のとき、未コミットの変更が残っていれば **git 自身が拒否する**
/// (ここで握り潰さずエラーを返す)。ブランチの削除も `-d` (マージ済みのみ) を
/// 先に試し、駄目なら `force` のときだけ `-D` を撃つ。
pub fn remove_agent_worktree(wt: &AgentWorktree, force: bool) -> Result<(), String> {
    let dir_s = wt.dir.to_string_lossy().into_owned();
    let args = worktree_remove_args(&dir_s, force);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    git_out(&wt.repo, &argv)?;
    let _ = git_out(&wt.repo, &["worktree", "prune"]);
    if git_out(&wt.repo, &["branch", "-d", &wt.branch]).is_err() && force {
        let _ = git_out(&wt.repo, &["branch", "-D", &wt.branch]);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 触ったファイルの走査
// ---------------------------------------------------------------------------

/// `git status --porcelain=v1 -z` の 1 レコード。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    /// 状態 2 文字 (index, worktree)。例: `" M"` / `"??"` / `"R "`。
    pub xy: String,
    /// 触れたパス。リネーム / コピーは (新, 旧) の 2 本になる。
    pub paths: Vec<PathBuf>,
}

impl StatusEntry {
    /// 未追跡ファイルか (`??`)。
    pub fn is_untracked(&self) -> bool {
        self.xy == "??"
    }
}

/// `git status --porcelain=v1 -z` をパースする。
///
/// `-z` にする理由は 2 つ: (1) 空白や日本語を含むパスが引用符でエスケープされない、
/// (2) 改行を含むパスでもレコードが壊れない。レコードは `XY<空白>PATH\0` で、
/// リネーム / コピーのときだけ直後に旧パスの NUL 終端フィールドが 1 本続く。
pub fn parse_status_z(raw: &str) -> Vec<StatusEntry> {
    let mut out = Vec::new();
    let mut it = raw.split('\0');
    while let Some(field) = it.next() {
        let b = field.as_bytes();
        // "XY PATH" に満たないもの (末尾の空フィールド等) は読み飛ばす。
        if b.len() < 4 || b[2] != b' ' {
            continue;
        }
        let xy = field[..2].to_string();
        let mut paths = vec![PathBuf::from(&field[3..])];
        // R/C は「新パス」に続けて「旧パス」が別フィールドで来る。両方が
        // 触られたファイルなので両方を数える (旧パスは削除として衝突しうる)。
        let renamed = b[..2].iter().any(|c| *c == b'R' || *c == b'C');
        if renamed {
            if let Some(orig) = it.next().filter(|s| !s.is_empty()) {
                paths.push(PathBuf::from(orig));
            }
        }
        out.push(StatusEntry { xy, paths });
    }
    out
}

/// `git diff --name-only -z` (NUL 区切り) の出力をパスの列にする。
pub fn parse_name_only_z(raw: &str) -> Vec<PathBuf> {
    raw.split('\0')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// 作業ツリーの `status` レコード (未コミット + 未追跡)。
pub fn status_entries(dir: &Path) -> Result<Vec<StatusEntry>, String> {
    Ok(parse_status_z(&git_out(
        dir,
        &["status", "--porcelain=v1", "-z"],
    )?))
}

/// 作業ツリーで触られているファイル (リポジトリ相対パス)。
///
/// `base` を渡すと `<base>...HEAD` のコミット済み差分も合流させる
/// (レースの racer 用)。通常の Cockpit では `None` — 「いま作業中のファイル」
/// だけが取り合いの対象で、過去のコミットは既に決着しているため。
///
/// **必ずワーカースレッドから呼ぶこと** (UI スレッドで git を待たない)。
pub fn scan_touched(dir: &Path, base: Option<&str>) -> Result<HashSet<PathBuf>, String> {
    let mut touched: HashSet<PathBuf> = status_entries(dir)?
        .into_iter()
        .flat_map(|e| e.paths)
        .collect();
    if let Some(base) = base {
        let range = format!("{base}...HEAD");
        touched.extend(parse_name_only_z(&git_out(
            dir,
            &["diff", "--name-only", "-z", &range],
        )?));
    }
    Ok(touched)
}

// ---------------------------------------------------------------------------
// 衝突の畳み込み
// ---------------------------------------------------------------------------

/// 触ったファイルの重なり。添字は呼び出し側が渡した並びの添字。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct OverlapReport {
    /// 2 体以上が触っているファイル → 触っている添字 (昇順)。
    /// 1 体しか触っていないファイルは載らない (= 安全なので見せる必要がない)。
    pub contended: BTreeMap<PathBuf, Vec<usize>>,
    /// 重なりのある組 `(小さい添字, 大きい添字, 重なったファイル)`。添字順・パス順。
    pub pairs: Vec<(usize, usize, Vec<PathBuf>)>,
}

impl OverlapReport {
    /// 誰ともぶつかっていないか。
    pub fn is_clean(&self) -> bool {
        self.contended.is_empty()
    }

    /// 2 体以上で競合しているファイルの本数。
    pub fn contended_count(&self) -> usize {
        self.contended.len()
    }

    /// `idx` の相手と、その相手と重なったファイル (相手の添字順)。
    pub fn for_racer(&self, idx: usize) -> Vec<(usize, Vec<PathBuf>)> {
        self.pairs
            .iter()
            .filter_map(|(a, b, files)| match (*a == idx, *b == idx) {
                (true, _) => Some((*b, files.clone())),
                (_, true) => Some((*a, files.clone())),
                _ => None,
            })
            .collect()
    }

    /// `idx` が誰かと取り合っているファイルの合併 (昇順・重複なし)。
    pub fn files_for(&self, idx: usize) -> Vec<PathBuf> {
        self.contended
            .iter()
            .filter(|(_, who)| who.contains(&idx))
            .map(|(p, _)| p.clone())
            .collect()
    }
}

/// 「触ったファイル集合」から衝突を畳む。
///
/// 入力は `(添字, 集合)` の列で、破棄済みなど対象外は呼び出し側で落としておく。
/// 同じ添字が 2 度来ることは想定しない。
pub fn compute_overlaps(sets: &[(usize, HashSet<PathBuf>)]) -> OverlapReport {
    // ファイル → 触った添字。BTreeMap なので出力順は常にパス昇順で決定的。
    let mut by_file: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
    for (idx, files) in sets {
        for f in files {
            by_file.entry(f.clone()).or_default().push(*idx);
        }
    }
    let mut contended: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
    // 組 → 取り合っているファイル。contended から作るので両者は必ず整合する。
    let mut pair_files: BTreeMap<(usize, usize), Vec<PathBuf>> = BTreeMap::new();
    for (file, who) in by_file {
        if who.len() < 2 {
            continue;
        }
        let mut who = who;
        who.sort_unstable();
        who.dedup();
        if who.len() < 2 {
            continue;
        }
        for (i, a) in who.iter().enumerate() {
            for b in &who[i + 1..] {
                pair_files.entry((*a, *b)).or_default().push(file.clone());
            }
        }
        contended.insert(file, who);
    }
    let pairs = pair_files
        .into_iter()
        .map(|((a, b), files)| (a, b, files))
        .collect();
    OverlapReport { contended, pairs }
}

/// このプラットフォームのファイルシステムが大文字小文字を区別しないか。
///
/// macOS (既定の APFS/HFS+) と Windows は区別しない。そこで `SRC/App.rs` と
/// `src/app.rs` は **同じファイル** なので、衝突判定でも同じ鍵に畳まないと
/// 「別ファイル扱いで衝突を見落とす」ことになる。
pub fn fs_case_insensitive() -> bool {
    cfg!(any(target_os = "macos", target_os = "ios", windows))
}

/// 衝突判定に使うパスの鍵。大文字小文字を区別しない FS では小文字へ畳む。
///
/// セパレータも `/` へ寄せる (Windows の `src\app.rs` と git 由来の
/// `src/app.rs` を同じ鍵にするため)。
pub fn path_key(p: &Path) -> PathBuf {
    let s = p.to_string_lossy().replace('\\', "/");
    if fs_case_insensitive() {
        PathBuf::from(s.to_lowercase())
    } else {
        PathBuf::from(s)
    }
}

/// 競合している 1 ファイル。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictFile {
    /// 表示用の相対パス (作業ツリーからの相対)。
    pub label: String,
    /// 突き合わせに使った鍵 (cwd を結合した正規化済み絶対パス)。
    pub key: PathBuf,
    /// 取り合っているセッション ID (昇順)。
    pub agents: Vec<u64>,
}

/// 通常運用の衝突レポート (セッション ID ベース)。
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct ConflictReport {
    /// 競合ファイル (表示ラベル昇順)。
    pub files: Vec<ConflictFile>,
}

impl ConflictReport {
    /// 衝突が 1 件も無いか。
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// 競合しているファイル数 (バッジの数字)。
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// 巻き込まれているセッション ID (昇順・重複なし)。
    pub fn agents(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.files.iter().flat_map(|f| f.agents.clone()).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// `id` が誰かとファイルを取り合っているか (カードの ⚠ 判定)。
    pub fn has_agent(&self, id: u64) -> bool {
        self.files.iter().any(|f| f.agents.contains(&id))
    }

    /// `id` が取り合っているファイルのラベル (昇順)。ツールチップ用。
    pub fn labels_for(&self, id: u64) -> Vec<String> {
        self.files
            .iter()
            .filter(|f| f.agents.contains(&id))
            .map(|f| f.label.clone())
            .collect()
    }
}

/// `(セッション ID, 作業ツリー, 触ったファイル (相対))` から衝突を畳む純粋関数。
///
/// 鍵は `cwd + 相対パス` を [`path_key`] で正規化したもの。だから
/// **worktree で隔離した 2 体が同じ `src/app.rs` を編集していても衝突しない**
/// (cwd が違えば鍵が違う) 一方、同じツリーに同居していれば必ず衝突として出る。
pub fn build_conflicts(sets: &[(u64, PathBuf, HashSet<PathBuf>)]) -> ConflictReport {
    let idx_sets: Vec<(usize, HashSet<PathBuf>)> = sets
        .iter()
        .enumerate()
        .map(|(i, (_, cwd, files))| (i, files.iter().map(|f| path_key(&cwd.join(f))).collect()))
        .collect();
    let overlaps = compute_overlaps(&idx_sets);
    // 鍵 → 表示ラベル (最初に見つかった相対パス)。
    let mut labels: HashMap<PathBuf, String> = HashMap::new();
    for (_, cwd, files) in sets {
        for f in files {
            labels
                .entry(path_key(&cwd.join(f)))
                .or_insert_with(|| f.to_string_lossy().replace('\\', "/"));
        }
    }
    let mut files: Vec<ConflictFile> = overlaps
        .contended
        .into_iter()
        .map(|(key, who)| {
            let mut agents: Vec<u64> = who.into_iter().map(|i| sets[i].0).collect();
            agents.sort_unstable();
            agents.dedup();
            let label = labels
                .get(&key)
                .cloned()
                .unwrap_or_else(|| key.to_string_lossy().into_owned());
            ConflictFile { label, key, agents }
        })
        .filter(|f| f.agents.len() >= 2)
        .collect();
    files.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.key.cmp(&b.key)));
    ConflictReport { files }
}

/// 同じ作業ツリーに **2 体以上の稼働中エージェント** が居るグループだけを返す。
///
/// これが空なら走査は 1 回も起きない。単独作業や worktree 隔離だけの構成では
/// git を叩かないので、アイドル時のコストは文字通りゼロになる。
pub fn shared_cwd_groups(agents: &[(u64, PathBuf, bool)]) -> Vec<(PathBuf, Vec<u64>)> {
    let mut by_dir: BTreeMap<PathBuf, (PathBuf, Vec<u64>)> = BTreeMap::new();
    for (id, cwd, running) in agents {
        if !*running {
            continue;
        }
        let e = by_dir
            .entry(path_key(cwd))
            .or_insert_with(|| (cwd.clone(), Vec::new()));
        e.1.push(*id);
    }
    by_dir
        .into_values()
        .filter_map(|(dir, mut ids)| {
            ids.sort_unstable();
            ids.dedup();
            (ids.len() >= 2).then_some((dir, ids))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 監視 (別スレッド + TTL + try_recv)
// ---------------------------------------------------------------------------

type ScanResult = Vec<(PathBuf, HashSet<PathBuf>)>;

/// 同居エージェントのファイル衝突を見張る。
///
/// - **同じ cwd に 2 体以上居るときだけ**走査する。居なければ git を叩かない。
/// - 走査は別スレッド。UI スレッドは `try_recv` するだけで待たない。
/// - 再走査は「顔ぶれが変わったとき」か [`SCAN_TTL`] 経過後だけ。
///   毎フレーム git を起こしたりはしない。
/// - 自分から `request_repaint` を呼ばない (常時アニメーションを作らない)。
#[derive(Default)]
pub struct ConflictWatch {
    report: ConflictReport,
    /// 直近に走査した cwd → 触ったファイル。
    cache: HashMap<PathBuf, HashSet<PathBuf>>,
    /// いま見張っているグループ (cwd, セッション ID 昇順)。
    watched: Vec<(PathBuf, Vec<u64>)>,
    rx: Option<Receiver<ScanResult>>,
    last: Option<Instant>,
}

impl ConflictWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// 現在のエージェント `(セッション ID, cwd, 稼働中か)` を渡して 1 段進める。
    /// Cockpit / 看板を描くときにだけ呼ぶ (閉じている間は 1 命令も走らない)。
    pub fn update(&mut self, agents: &[(u64, PathBuf, bool)]) {
        let groups = shared_cwd_groups(agents);
        if groups.is_empty() {
            // 見張る相手が居ない = 走査も保持もしない。前回の警告は消す。
            if !self.watched.is_empty() || !self.report.is_empty() {
                self.watched.clear();
                self.cache.clear();
                self.report = ConflictReport::default();
                self.rx = None;
                self.last = None;
            }
            return;
        }
        let changed = groups != self.watched;
        if changed {
            self.watched = groups;
        }
        // 走査結果の取り込み (待たない)。
        let mut got = false;
        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(res) => {
                    for (dir, files) in res {
                        self.cache.insert(dir, files);
                    }
                    self.rx = None;
                    got = true;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.rx = None,
            }
        }
        if got || changed {
            self.rebuild();
        }
        let due = changed || self.last.is_none_or(|t| t.elapsed() >= SCAN_TTL);
        if self.rx.is_none() && due {
            self.last = Some(Instant::now());
            let dirs: Vec<PathBuf> = self.watched.iter().map(|(d, _)| d.clone()).collect();
            let (tx, rx) = channel();
            std::thread::spawn(move || {
                let out: ScanResult = dirs
                    .into_iter()
                    .map(|d| {
                        let files = scan_touched(&d, None).unwrap_or_default();
                        (d, files)
                    })
                    .collect();
                let _ = tx.send(out);
            });
            self.rx = Some(rx);
        }
    }

    /// いまの衝突レポート。
    pub fn report(&self) -> &ConflictReport {
        &self.report
    }

    fn rebuild(&mut self) {
        let sets: Vec<(u64, PathBuf, HashSet<PathBuf>)> = self
            .watched
            .iter()
            .flat_map(|(dir, ids)| {
                let files = self.cache.get(dir).cloned().unwrap_or_default();
                ids.iter()
                    .map(move |id| (*id, dir.clone(), files.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        self.report = build_conflicts(&sets);
    }
}

/// 衝突ファイルの一覧を 1 行へ畳む (バッジのツールチップ用)。
/// `max` 本まで並べ、残りは「他 N 件」に畳む。
pub fn summarize_labels(labels: &[String], max: usize) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let head: Vec<String> = labels.iter().take(max).cloned().collect();
    let rest = labels.len().saturating_sub(head.len());
    if rest == 0 {
        head.join(", ")
    } else {
        format!(
            "{}{}",
            head.join(", "),
            trf(" 他 {n} 件", &[("n", rest.to_string())])
        )
    }
}

/// worktree を消す前にユーザーへ見せる本文。`dirty` なら失うものを明示する。
pub fn removal_prompt(branch: &str, dir: &Path, dirty: bool) -> String {
    let head = trf(
        "エージェント専用の worktree ({branch}) が残っています。\n{dir}",
        &[
            ("branch", branch.to_string()),
            ("dir", dir.display().to_string()),
        ],
    );
    if dirty {
        format!(
            "{head}\n\n{}",
            tr("⚠ 未コミットの変更があります。削除するとその変更は失われます。")
        )
    } else {
        format!("{head}\n\n{}", tr("未コミットの変更はありません。"))
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fset(paths: &[&str]) -> HashSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    // ── スラグ / 命名 ────────────────────────────────────────────────

    #[test]
    fn スラグはスペースとスラッシュを潰す() {
        assert_eq!(slugify("My Cool Repo", "x"), "my-cool-repo");
        assert_eq!(slugify("feature/login ui", "x"), "feature-login-ui");
        assert_eq!(slugify("  --leading--  ", "x"), "leading");
    }

    #[test]
    fn 日本語だけのフォルダ名はフォールバックへ落ちる() {
        assert_eq!(slugify("作業フォルダ", "agent"), "agent");
        assert_eq!(slugify("🚀🚀", "agent"), "agent");
        // 日本語混じりでも ASCII 部分は残る
        assert_eq!(slugify("私の repo 2", "agent"), "repo-2");
    }

    #[test]
    fn スラグは上限で切られる() {
        let s = slugify(&"a".repeat(100), "x");
        assert!(s.len() <= SLUG_MAX, "{s} が長すぎる");
    }

    #[test]
    fn ブランチ名とフォルダ名の形() {
        assert_eq!(agent_branch("my-repo", 3), "agent/my-repo-3");
        assert_eq!(
            agent_dir_name("zaivern", "my-repo", 3),
            "zaivern-agent-my-repo-3"
        );
    }

    #[test]
    fn 空きスロットは既存を避けて連番を進める() {
        let taken: HashSet<&str> = ["agent/proj-1", "agent/proj-2"].into_iter().collect();
        let (b, d) = pick_agent_slot("proj", "repo", |br, _| taken.contains(br));
        assert_eq!(b, "agent/proj-3");
        assert_eq!(d, "repo-agent-proj-3");
    }

    #[test]
    fn 空きスロットはフォルダ側の衝突も避ける() {
        // ブランチは空いているがフォルダだけ残っている中途半端な状態。
        let (b, d) = pick_agent_slot("proj", "repo", |_, dir| dir == "repo-agent-proj-1");
        assert_eq!(b, "agent/proj-2");
        assert_eq!(d, "repo-agent-proj-2");
    }

    #[test]
    fn 生成される名前はユーザー名にもホームパスにも依存しない() {
        // ホームや長い絶対パスを与えても、名前に混ざるのは「最後の要素のスラグ」だけ。
        let repo = PathBuf::from("/home/someone-else/work/My Repo");
        let repo_name = repo.file_name().unwrap().to_string_lossy().into_owned();
        let slug = slugify(&repo_name, "repo");
        let (b, d) = pick_agent_slot(&slug, &slug, |_, _| false);
        assert_eq!(b, "agent/my-repo-1");
        assert_eq!(d, "my-repo-agent-my-repo-1");
        for s in [&b, &d] {
            assert!(!s.contains("someone-else"), "{s} にユーザー名が混ざった");
            assert!(!s.contains('/') || s.starts_with("agent/"), "{s}");
            assert!(!s.contains("home"), "{s} にホームパスが混ざった");
        }
    }

    #[test]
    fn 置き場はリポジトリの隣でclaude管理下だけ退避する() {
        let base = worktree_base(Path::new("/a/b/repo"), "leaf");
        assert_eq!(base, PathBuf::from("/a/b"));
        let esc = worktree_base(Path::new("/a/.claude/worktrees/w1"), "leaf");
        assert_eq!(esc, std::env::temp_dir().join("leaf"));
    }

    #[test]
    fn remove引数はforceのときだけ強制を付ける() {
        assert_eq!(
            worktree_remove_args("/d", false),
            vec!["worktree", "remove", "/d"]
        );
        assert_eq!(
            worktree_remove_args("/d", true),
            vec!["worktree", "remove", "--force", "/d"]
        );
    }

    // ── git リポジトリ判定 ──────────────────────────────────────────

    #[test]
    fn gitリポジトリでないフォルダは隔離を許さない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-wt-test", "nogit");
        let sub = dir.join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(!looks_like_git_repo(&sub), "git でないのに true");
        // 実際の作成も断る (サブプロセス経由の本番判定)。
        assert!(create_agent_worktree(&sub, "claude").is_err());
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert!(looks_like_git_repo(&sub), "祖先の .git を見つけられない");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 衝突の畳み込み ──────────────────────────────────────────────

    #[test]
    fn 衝突表() {
        struct Case {
            name: &'static str,
            sets: Vec<(u64, &'static str, Vec<&'static str>)>,
            files: usize,
            agents: Vec<u64>,
        }
        let cases = vec![
            Case {
                name: "0 体 — 何も出ない",
                sets: vec![],
                files: 0,
                agents: vec![],
            },
            Case {
                name: "1 体 — 単独なら衝突ではない",
                sets: vec![(1, "/w", vec!["src/a.rs", "src/b.rs"])],
                files: 0,
                agents: vec![],
            },
            Case {
                name: "2 体が同じファイル",
                sets: vec![
                    (1, "/w", vec!["src/a.rs", "src/x.rs"]),
                    (2, "/w", vec!["src/a.rs", "src/y.rs"]),
                ],
                files: 1,
                agents: vec![1, 2],
            },
            Case {
                name: "2 体だが交わらない",
                sets: vec![(1, "/w", vec!["src/x.rs"]), (2, "/w", vec!["src/y.rs"])],
                files: 0,
                agents: vec![],
            },
            Case {
                name: "3 体以上が同じファイル",
                sets: vec![
                    (1, "/w", vec!["src/a.rs"]),
                    (2, "/w", vec!["src/a.rs"]),
                    (3, "/w", vec!["src/a.rs", "src/b.rs"]),
                    (4, "/w", vec!["src/b.rs"]),
                ],
                files: 2,
                agents: vec![1, 2, 3, 4],
            },
            Case {
                name: "worktree 隔離済みは同じ相対パスでも衝突しない",
                sets: vec![
                    (1, "/w-agent-1", vec!["src/app.rs"]),
                    (2, "/w-agent-2", vec!["src/app.rs"]),
                ],
                files: 0,
                agents: vec![],
            },
        ];
        for c in cases {
            let sets: Vec<(u64, PathBuf, HashSet<PathBuf>)> = c
                .sets
                .iter()
                .map(|(id, cwd, f)| (*id, PathBuf::from(cwd), fset(f)))
                .collect();
            let rep = build_conflicts(&sets);
            assert_eq!(rep.file_count(), c.files, "{}: ファイル数", c.name);
            assert_eq!(rep.agents(), c.agents, "{}: 巻き込まれた ID", c.name);
            assert_eq!(rep.is_empty(), c.files == 0, "{}: is_empty", c.name);
        }
    }

    #[test]
    fn 三体の競合はどのファイルで誰が当たっているかまで分かる() {
        let sets = vec![
            (7, PathBuf::from("/w"), fset(&["src/a.rs"])),
            (8, PathBuf::from("/w"), fset(&["src/a.rs"])),
            (9, PathBuf::from("/w"), fset(&["src/a.rs"])),
        ];
        let rep = build_conflicts(&sets);
        assert_eq!(rep.files.len(), 1);
        assert_eq!(rep.files[0].label, "src/a.rs");
        assert_eq!(rep.files[0].agents, vec![7, 8, 9]);
        assert!(rep.has_agent(8));
        assert!(!rep.has_agent(10));
        assert_eq!(rep.labels_for(9), vec!["src/a.rs".to_string()]);
    }

    #[test]
    fn 大文字小文字の扱いはファイルシステムに合わせる() {
        let sets = vec![
            (1, PathBuf::from("/w"), fset(&["src/App.rs"])),
            (2, PathBuf::from("/w"), fset(&["src/app.rs"])),
        ];
        let rep = build_conflicts(&sets);
        if fs_case_insensitive() {
            // macOS / Windows: 同じファイルなので必ず衝突として出す。
            assert_eq!(rep.file_count(), 1, "大小無視 FS で見落とした");
            assert_eq!(rep.files[0].agents, vec![1, 2]);
        } else {
            // Linux: 別ファイルなので衝突ではない。
            assert_eq!(rep.file_count(), 0, "大小区別 FS で誤検出した");
        }
    }

    #[test]
    fn パス区切りはどちらのosの表記でも同じ鍵になる() {
        let sets = vec![
            (1, PathBuf::from("/w"), fset(&["src/a.rs"])),
            (2, PathBuf::from("/w"), fset(&["src\\a.rs"])),
        ];
        assert_eq!(build_conflicts(&sets).file_count(), 1);
    }

    #[test]
    fn 監視対象は同じツリーに二体以上いるときだけ() {
        let a = PathBuf::from("/w");
        let b = PathBuf::from("/w-agent-1");
        // 1 体だけ → 監視しない (= git を 1 回も叩かない)
        assert!(shared_cwd_groups(&[(1, a.clone(), true)]).is_empty());
        // 2 体だが片方が終了済み → 監視しない
        assert!(shared_cwd_groups(&[(1, a.clone(), true), (2, a.clone(), false)]).is_empty());
        // 別ツリー同士 → 監視しない
        assert!(shared_cwd_groups(&[(1, a.clone(), true), (2, b.clone(), true)]).is_empty());
        // 同じツリーに 2 体 → 監視する
        let g = shared_cwd_groups(&[(2, a.clone(), true), (1, a.clone(), true)]);
        assert_eq!(g, vec![(a, vec![1, 2])]);
    }

    #[test]
    fn 監視は相手が居なくなったら警告ごと畳む() {
        let mut w = ConflictWatch::new();
        assert!(w.report().is_empty());
        // 単独なら走査自体が起きない (レポートは空のまま)。
        w.update(&[(1, PathBuf::from("/nonexistent-tree"), true)]);
        assert!(w.report().is_empty());
    }

    #[test]
    fn 一覧の要約は上限で畳む() {
        let labels: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let s = summarize_labels(&labels, 2);
        assert!(s.starts_with("a, b"), "{s}");
        assert!(s.contains('2'), "{s}");
        assert_eq!(summarize_labels(&labels[..1], 2), "a");
        assert_eq!(summarize_labels(&[], 2), "");
    }

    #[test]
    fn 削除確認の本文は未コミット変更を明示する() {
        let dirty = removal_prompt("agent/x-1", Path::new("/w"), true);
        assert!(dirty.contains("agent/x-1"));
        assert!(dirty.contains("失われ"), "{dirty}");
        let clean = removal_prompt("agent/x-1", Path::new("/w"), false);
        assert!(!clean.contains("失われ"), "{clean}");
    }

    // ── パーサ ──────────────────────────────────────────────────────

    #[test]
    fn statusのzパースはリネームの両側を拾う() {
        let raw = " M src/a.rs\0?? new.txt\0R  dst.rs\0src/orig.rs\0";
        let e = parse_status_z(raw);
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].paths, vec![PathBuf::from("src/a.rs")]);
        assert!(e[1].is_untracked());
        assert_eq!(
            e[2].paths,
            vec![PathBuf::from("dst.rs"), PathBuf::from("src/orig.rs")]
        );
    }

    #[test]
    fn name_onlyのzパースは空フィールドを捨てる() {
        assert_eq!(
            parse_name_only_z("a\0b\0\0"),
            vec![PathBuf::from("a"), PathBuf::from("b")]
        );
        assert!(parse_name_only_z("").is_empty());
    }

    // ── 実 git を使う結合テスト ──────────────────────────────────────

    /// 実 git で最小のリポジトリを作る。git が無い環境では None を返して黙って飛ばす。
    fn fixture_repo(tag: &str) -> Option<PathBuf> {
        // リポジトリを**一段深く**掘る理由は `race.rs` の同名関数と同じ。
        // `worktree_base` は `repo.parent()` を置き場にするので、
        // リポジトリを一時ディレクトリ直下に作ると worktree が共有の
        // $TMPDIR 直下へ生まれ、並列実行時にディレクトリロックで詰まる。
        let base = crate::test_util::unique_temp_dir("zaivern-wt-test", tag);
        let root = base.join("repo");
        if std::fs::create_dir_all(&root).is_err() {
            return None;
        }
        let ok = |args: &[&str]| git_out(&root, args).is_ok();
        if crate::procx::hidden_command("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return None;
        }
        std::fs::write(root.join("a.txt"), "hello\n").ok()?;
        if !ok(&["init", "-q"])
            || !ok(&["config", "user.email", "t@example.com"])
            || !ok(&["config", "user.name", "t"])
            || !ok(&["add", "-A"])
            || !ok(&["commit", "-q", "-m", "init"])
        {
            let _ = std::fs::remove_dir_all(&base);
            return None;
        }
        Some(root)
    }

    #[test]
    fn worktreeを作って触ったファイルを拾い消せる() {
        let Some(repo) = fixture_repo("create") else {
            return;
        };
        let wt = create_agent_worktree(&repo, "Claude Code").expect("worktree 作成");
        assert!(wt.dir.is_dir(), "worktree のフォルダが無い");
        assert_eq!(wt.branch, "agent/claude-code-1");
        // 同じラベルで 2 本目を作ると連番が進む (名前が衝突しない)。
        let wt2 = create_agent_worktree(&repo, "Claude Code").expect("2 本目");
        assert_eq!(wt2.branch, "agent/claude-code-2");
        assert_ne!(wt.dir, wt2.dir);

        // 触ったファイルが相対パスで拾える。
        assert!(!worktree_is_dirty(&wt.dir), "作りたては綺麗なはず");
        std::fs::write(wt.dir.join("a.txt"), "changed\n").unwrap();
        std::fs::write(wt.dir.join("新規 ファイル.txt"), "x").unwrap();
        assert!(worktree_is_dirty(&wt.dir));
        let touched = scan_touched(&wt.dir, None).expect("scan");
        assert!(touched.contains(&PathBuf::from("a.txt")), "{touched:?}");
        assert!(
            touched.contains(&PathBuf::from("新規 ファイル.txt")),
            "空白と日本語を含むパスが壊れた: {touched:?}"
        );

        // 変更が残っていると force 無しでは消せない (git 自身が拒否する)。
        assert!(
            remove_agent_worktree(&wt, false).is_err(),
            "汚れたまま消えた"
        );
        assert!(wt.dir.is_dir(), "拒否されたのに消えている");
        remove_agent_worktree(&wt, true).expect("force なら消える");
        assert!(!wt.dir.is_dir());
        // 綺麗な方は force 無しで消える。
        remove_agent_worktree(&wt2, false).expect("綺麗なら消える");
        assert!(!wt2.dir.is_dir());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn 同居する二体は同じ変更ファイルを取り合っていると出る() {
        let Some(repo) = fixture_repo("share") else {
            return;
        };
        std::fs::write(repo.join("a.txt"), "edited\n").unwrap();
        let touched = scan_touched(&repo, None).expect("scan");
        let sets = vec![
            (1u64, repo.clone(), touched.clone()),
            (2u64, repo.clone(), touched),
        ];
        let rep = build_conflicts(&sets);
        assert_eq!(rep.file_count(), 1, "{rep:?}");
        assert_eq!(rep.files[0].label, "a.txt");
        assert_eq!(rep.files[0].agents, vec![1, 2]);
        let _ = std::fs::remove_dir_all(&repo);
    }
}
