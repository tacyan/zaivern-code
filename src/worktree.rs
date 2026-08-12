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

// ═══════════════════════════════════════════════════════════════════════════
//  大文字小文字を畳むか — **実ファイルシステムに訊く**
//
//  かつてここは `cfg!(any(target_os = "macos", windows))` だった。つまり
//  **コンパイル時の OS** を FS の性質の代用にしていた。この 2 つは別物である:
//
//  * Linux でも case-insensitive なマウントは普通にある
//    (ciopfs / ntfs-3g / exFAT / SMB / macOS 由来のディスクイメージ)
//  * macOS でも case-sensitive な APFS ボリュームを作れる (`hdiutil`)
//  * Windows は `fsutil file setCaseSensitiveInfo` で**ディレクトリ単位**に変わる
//
//  食い違うと **同じファイルに 2 つの台帳キー**ができ、「同じ行を 2 人に
//  配らない」という中心的な保証が静かに崩れる。台帳の最終形に重なりは
//  残らないので、台帳を見ても気付けない。
// ═══════════════════════════════════════════════════════════════════════════

/// 実 FS を叩いて**観測したこと**。判定 ([`judge_case_probe`]) と分けてある。
///
/// 分ける理由は 1 つ: **大小を区別するボリュームを持っていないホストからでも
/// 判定の両方の枝を固定できる**ようにするため (`czero_init::parse_scoped_config`
/// と同じ流儀)。I/O を含んだままでは、macOS で開発している限り
/// 「区別する側」の枝が一度も検査されない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseProbe {
    /// 綴りだけ変えた名前で開けて、中身も自分が書いた印だった = **非区別**。
    SameFile,
    /// 綴りを変えた名前では見つからなかった = **区別する**。
    Missing,
    /// 開けたが中身が違った (無関係な実体がたまたま居た) = **区別する**。
    OtherFile,
}

/// 観測 → 判定。**純関数**。`None` = 検査そのものができなかった。
///
/// ## 検査できないときに畳む側へ倒す理由
///
/// 読み取り専用・権限が無い・容量が無い、のいずれでも `None` になる。
/// このとき取れる向きは 2 つで、**壊れ方が対称ではない**:
///
/// * **畳まない**側へ倒すと、実際には非区別な FS で `src/Foo.rs` と
///   `src/foo.rs` が別リースになる = **同じ物理ファイルを 2 人が同時に持てる**。
///   中心の保証が破れる。しかも台帳には重なりが残らないので気付けない。
/// * **畳む**側へ倒すと、実際には区別する FS で別々の 2 ファイルが同じ鍵に
///   なる = **要らない待ちが 1 件増える**だけ。過剰に止める方向 (fail-closed)。
///
/// 前者は静かにデータを壊し、後者は目に見えて不便なだけなので、畳む側へ倒す。
pub fn judge_case_probe(obs: Option<CaseProbe>) -> bool {
    match obs {
        Some(CaseProbe::SameFile) => true,
        Some(CaseProbe::Missing | CaseProbe::OtherFile) => false,
        None => true,
    }
}

// テスト用のカウンタ。**プロセス共通の `static AtomicUsize` にしない** —
// 同時に走っている他のテストの検査まで混ざって数が合わなくなる。
thread_local! {
    static CASE_PROBE_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// このスレッドが実 I/O の検査を行った回数。**費用を絶対時間ではなく
/// 呼び出し回数で見る**ため (負荷で散る数字は線を引く材料にならない)。
#[cfg(test)]
pub fn case_probe_calls() -> u64 {
    CASE_PROBE_CALLS.with(std::cell::Cell::get)
}

/// 検査でファイルを置く場所。
///
/// `dir` に `.git` ディレクトリがあればその中を使う。同じボリューム
/// (= 同じ大小の性質) でありながら、**git が中身を一切報告しない**ので
/// 作業ツリーに一瞬でもゴミが見えない。`gate` は書き込みのたびに走る
/// 短命プロセスなので、リポジトリ直下に作ると監視側 (このアプリ自身の
/// git スキャンを含む) が毎回反応してしまう。
///
/// linked worktree の `.git` は**ファイル**なので、その場合は `dir` 自身。
fn case_probe_dir(dir: &Path) -> PathBuf {
    let dot = dir.join(".git");
    if dot.is_dir() {
        dot
    } else {
        dir.to_path_buf()
    }
}

/// 実 FS を 1 度だけ叩いて観測する。**記憶しない** (記憶は [`CaseOracle`])。
///
/// 綴りだけが違う 2 つの名前を使い、片方で書いて**もう片方で読めるか**を見る。
/// 存在検査 (`exists`) だけでは足りない — 無関係な同名ファイルが居たときに
/// 「非区別」と誤判定するため、書いた印を読み返して同一性まで確かめる。
pub fn probe_case_at(dir: &Path) -> Option<CaseProbe> {
    CASE_PROBE_CALLS.with(|c| c.set(c.get().saturating_add(1)));
    let base = case_probe_dir(dir);
    // 名前は **ASCII の大小だけ**が違う 2 つ。残りは pid + 時刻 + 連番なので
    // 数字と `-` しか含まず、綴り替えの影響を受けない。
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let uniq = format!("{}-{}-{}", std::process::id(), nanos, seq);
    let written = base.join(format!(".zaivern-case-{uniq}"));
    let flipped = base.join(format!(".zaivern-CASE-{uniq}"));
    std::fs::write(&written, uniq.as_bytes()).ok()?;
    let obs = match std::fs::read(&flipped) {
        Ok(got) if got == uniq.as_bytes() => CaseProbe::SameFile,
        Ok(_) => CaseProbe::OtherFile,
        Err(_) => CaseProbe::Missing,
    };
    // 短命プロセスが途中で死んでも置き去りが 1 個で済むよう、必ず消す。
    let _ = std::fs::remove_file(&written);
    Some(obs)
}

/// 検査結果の記憶。**同じ場所を 2 度検査しない。**
///
/// 台帳の鍵は書き込みのたびに作られるので、毎回 I/O すると全部が重くなる。
/// 記憶の単位は**ディレクトリ**である (Windows は大小の扱いがディレクトリ
/// 単位で変わるため、ボリューム単位に丸めると外す)。
#[derive(Default)]
pub struct CaseOracle {
    cache: std::sync::Mutex<HashMap<PathBuf, bool>>,
}

impl CaseOracle {
    /// この場所の FS が大文字小文字を区別しないか。初回だけ実 I/O。
    pub fn get(&self, dir: &Path) -> bool {
        // 検査を抱えたままロックを持つ。**ちょうど 1 回**にするためで、
        // 先に降りると 2 スレッドが同時に検査しうる (数え方が嘘になる)。
        let Ok(mut m) = self.cache.lock() else {
            // 毒された = どこかで panic した。記憶を諦めて毎回検査する
            // (遅いだけで答えは正しい)。
            return judge_case_probe(probe_case_at(dir));
        };
        if let Some(v) = m.get(dir) {
            return *v;
        }
        let v = judge_case_probe(probe_case_at(dir));
        m.insert(dir.to_path_buf(), v);
        v
    }
}

static CASE_ORACLE: std::sync::OnceLock<CaseOracle> = std::sync::OnceLock::new();

/// **この場所の** FS が大文字小文字を区別しないか (実測・記憶付き)。
pub fn fs_case_insensitive_at(dir: &Path) -> bool {
    CASE_ORACLE.get_or_init(CaseOracle::default).get(dir)
}

// プロセス内で 1 度だけ決まる答え。**途中で変わってはいけない** —
// パターン側 (`lease::segments`) と実パス側 (`lease::rel_within`) が
// 別の答えを使うと、鍵が食い違って確保が素通りする。
static CASE_FOLD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// 台帳とコンフリクト判定で大文字小文字を畳むか。
///
/// **プロセス内で 1 度だけ実 FS を検査して固定する。**「1 度だけ」は費用の
/// 話ではなく**正しさ**の話で、途中で答えが変われば同じファイルに 2 つの
/// 鍵ができる。既知の作業ツリーがあるなら [`seed_fs_case`] で先に固定する
/// こと (誰も固定しなければ、初回の呼び出し時に現在地を検査する)。
///
/// ## 担保できないこと (正直に)
///
/// 答えはプロセスに 1 つなので、**1 つのプロセスが性質の違う 2 つの
/// ボリュームを同時に扱う場合**、後から来た側は最初に固定した答えを使う。
/// 台帳は作業ツリーごとに分かれているので実害は「片方の台帳で畳み方が
/// 過剰／不足」だが、ゼロではない。場所ごとの答えが要る呼び出しは
/// [`fs_case_insensitive_at`] を使う。
pub fn fs_case_insensitive() -> bool {
    *CASE_FOLD.get_or_init(|| fs_case_insensitive_at(&default_probe_dir()))
}

/// 既知の作業ツリーで [`fs_case_insensitive`] を固定する。
///
/// **最初の 1 回だけ効く**。返り値は固定された答え。
pub fn seed_fs_case(dir: &Path) -> bool {
    *CASE_FOLD.get_or_init(|| fs_case_insensitive_at(dir))
}

/// 誰も固定しなかったときに検査する場所。
///
/// 現在地 = `zai` / `gate` が起動された場所で、ほぼ必ず作業ツリーの中。
/// 取れなければ一時ディレクトリ (`TMPDIR` / `TEMP` を尊重するので、
/// パスの直書きにならない)。
fn default_probe_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir())
}

/// 衝突判定に使うパスの鍵。大文字小文字を区別しない FS では小文字へ畳む。
///
/// 「区別しないか」は [`fs_case_insensitive`] = **実 FS の実測**で決まる
/// (コンパイル時の OS ではない)。
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
mod case_tests {
    use super::{case_probe_calls, judge_case_probe, probe_case_at};
    use super::{CaseOracle, CaseProbe};
    use crate::test_util::unique_temp_dir;

    /// このディレクトリの FS が実際に大小を区別しないか。**テスト自身の目**で
    /// 見る (実装を一切通さない地の真実)。
    fn truth_at(dir: &std::path::Path) -> bool {
        let mixed = dir.join("Zz-truth.probe");
        std::fs::write(&mixed, b"x").expect("write truth probe");
        let seen = dir.join("zZ-truth.probe").exists();
        std::fs::remove_file(&mixed).ok();
        seen
    }

    /// 観測 → 判定の表。**大小を区別するボリュームを持っていないホストからでも
    /// 両方の枝を通る**のがこの表の目的。
    #[test]
    fn 観測から大小の判定を出す表() {
        let table: &[(Option<CaseProbe>, bool)] = &[
            (Some(CaseProbe::SameFile), true),
            (Some(CaseProbe::Missing), false),
            (Some(CaseProbe::OtherFile), false),
            // 検査できない → 畳む。畳みすぎは「余計に止める」だけだが、
            // 畳み損ねは「同じファイルに 2 つの鍵」= 中心の保証が壊れる。
            (None, true),
        ];
        for (obs, want) in table {
            assert_eq!(judge_case_probe(*obs), *want, "観測={obs:?}");
        }
    }

    /// **コンパイル時の OS ではなく実 FS に従う。**
    ///
    /// 旧実装は `cfg!(any(target_os = "macos", target_os = "ios", windows))`
    /// だった。macOS 上の case-sensitive ボリューム (hdiutil で作れる) や
    /// Linux 上の case-insensitive マウント (ciopfs / exFAT / SMB) では、
    /// この 2 つは食い違う。
    #[test]
    fn 大小の判定はコンパイル時のosではなく実fsに従う() {
        let dir = unique_temp_dir("zaivern-case", "fs");
        let truth = truth_at(&dir);
        let by_os = cfg!(any(target_os = "macos", target_os = "ios", windows));
        let by_fs = super::fs_case_insensitive_at(&dir);
        assert_eq!(
            by_fs,
            truth,
            "実 FS は大小を{}のに判定は{} (旧実装の OS 由来の答えは {by_os})",
            if truth {
                "区別しない"
            } else {
                "区別する"
            },
            by_fs
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 費用は**呼び出し回数**で見る。場所ごとにちょうど 1 回。
    ///
    /// 絶対時間で線を引かない (負荷で散る数字は材料にならない)。
    /// カウンタはスレッドローカルなので、同時に走る他のテストが混ざらない。
    #[test]
    fn 大小の検査は場所ごとにちょうど一度だけ() {
        let a = unique_temp_dir("zaivern-case", "once-a");
        let b = unique_temp_dir("zaivern-case", "once-b");
        // 記憶を独立させる (プロセス共通の記憶を使うと、他のテストが先に
        // 温めていたぶんが混ざって数が合わない)。
        let oracle = CaseOracle::default();
        let base = case_probe_calls();
        for _ in 0..500 {
            oracle.get(&a);
        }
        assert_eq!(
            case_probe_calls() - base,
            1,
            "同じ場所を 500 回訊いたのに検査が 1 回で済んでいない"
        );
        for _ in 0..500 {
            oracle.get(&b);
        }
        assert_eq!(
            case_probe_calls() - base,
            2,
            "場所ごとに記憶していない (Windows は大小の扱いがディレクトリ単位)"
        );
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    /// 検査は作業ツリーにゴミを残さない。`.git` があるならその中でやる
    /// (`gate` は書き込みのたびに走る短命プロセスなので、直下に作ると
    /// 監視側が毎回反応する)。
    #[test]
    fn 検査は作業ツリーを汚さない() {
        let dir = unique_temp_dir("zaivern-case", "clean");
        std::fs::create_dir_all(dir.join(".git")).expect("create .git");
        assert!(probe_case_at(&dir).is_some(), "検査そのものが失敗した");
        let top: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            top,
            vec![".git".to_string()],
            "作業ツリー直下にゴミが残った"
        );
        assert_eq!(
            std::fs::read_dir(dir.join(".git"))
                .expect("read .git")
                .count(),
            0,
            ".git の中にゴミが残った"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `.git` が無い場所でも検査でき、後に何も残らない。
    #[test]
    fn gitが無い場所でも検査できて何も残らない() {
        let dir = unique_temp_dir("zaivern-case", "nogit");
        assert!(probe_case_at(&dir).is_some());
        assert_eq!(std::fs::read_dir(&dir).expect("read dir").count(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 検査できない場所 (存在しない) は `None` = 畳む側へ倒れる。
    #[test]
    fn 検査できない場所は畳む側へ倒す() {
        let dir = unique_temp_dir("zaivern-case", "gone");
        std::fs::remove_dir_all(&dir).expect("remove");
        assert_eq!(probe_case_at(&dir), None);
        assert!(
            super::fs_case_insensitive_at(&dir),
            "fail-closed になっていない"
        );
    }

    /// プロセス共通の答えは**一度決まったら変わらない**。
    /// 変わると、パターン側と実パス側で鍵が食い違って確保が素通りする。
    #[test]
    fn プロセス共通の答えは変わらない() {
        let first = super::fs_case_insensitive();
        let dir = unique_temp_dir("zaivern-case", "seed");
        assert_eq!(
            super::seed_fs_case(&dir),
            first,
            "後から固定し直せてしまった"
        );
        assert_eq!(super::fs_case_insensitive(), first);
        std::fs::remove_dir_all(&dir).ok();
    }
}

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
