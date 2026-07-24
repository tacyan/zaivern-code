//! Git 連携モジュール。
//!
//! `git` CLI (std::process::Command) を用いてワークスペースのステータスと
//! 行単位の diff マークを取得する。git が無い場合や workspace が
//! git リポジトリでない場合は、すべて空 / None を返す。

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// status --porcelain=v1 から得たファイル単位のステータス。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileStatus {
    Modified,
    Added,
    Untracked,
    Deleted,
    Renamed,
}

/// エディタのガター等に表示する行マーク。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineMark {
    Added,
    Modified,
}

const STATUS_CACHE_TTL: Duration = Duration::from_secs(2);
const BRANCH_CACHE_TTL: Duration = Duration::from_secs(3);

/// 共有の空行マーク。repo 外・非 repo のバッファで毎フレーム返す値なので、
/// その都度 `Arc::new(Vec::new())` でアロケしないよう 1 つを使い回す。
pub(crate) fn empty_line_marks() -> Arc<Vec<(usize, LineMark)>> {
    static EMPTY: OnceLock<Arc<Vec<(usize, LineMark)>>> = OnceLock::new();
    Arc::clone(EMPTY.get_or_init(|| Arc::new(Vec::new())))
}

/// `dir` が属する git リポジトリのトップレベルを返す(非 repo / git 不在なら None)。
/// ルートがリポジトリのサブディレクトリでも正しいトップレベルが得られる。
pub fn discover_toplevel(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Some(PathBuf::from(s))
}

/// marks_cache の値: (text_hash, 行マーク)。
/// Arc 共有: キャッシュヒット時に Vec を複製しない。
type MarksEntry = (u64, Arc<Vec<(usize, LineMark)>>);

pub struct Git {
    workspace: PathBuf,
    /// 相対パス → ステータス (status --porcelain=v1 のパース結果)。
    status_cache: HashMap<String, FileStatus>,
    /// 最後に status を実行した時刻。None なら未実行。
    last_refresh: Option<Instant>,
    /// 相対パス → (text_hash, 行マーク) のキャッシュ。
    marks_cache: HashMap<String, MarksEntry>,
    /// ブランチ名の TTL キャッシュ (値, 取得時刻)。
    branch_cache: Option<(Option<String>, Instant)>,
}

impl Git {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            status_cache: HashMap::new(),
            last_refresh: None,
            marks_cache: HashMap::new(),
            branch_cache: None,
        }
    }

    pub fn set_workspace(&mut self, ws: PathBuf) {
        if self.workspace != ws {
            self.workspace = ws;
            self.status_cache.clear();
            self.marks_cache.clear();
            self.last_refresh = None;
            self.branch_cache = None;
        }
    }

    /// 現在のブランチ名 (3 秒 TTL キャッシュ)。detached HEAD なら短縮 SHA。
    ///
    /// `.git/HEAD` の直接パースではなく `git rev-parse` を使うため、
    /// worktree / submodule / `.git` がファイルのケースでも正しく動く。
    pub fn branch(&mut self) -> Option<String> {
        if let Some((v, at)) = &self.branch_cache {
            if at.elapsed() < BRANCH_CACHE_TTL {
                return v.clone();
            }
        }
        // `branch --show-current` は「まだ 1 コミットも無い (unborn HEAD)」でも
        // ブランチ名を返す。detached HEAD のときだけ空になるので、
        // その場合は短縮 SHA へフォールバックする。
        let name = self
            .run_git(&["branch", "--show-current"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.run_git(&["rev-parse", "--short", "HEAD"])
                    .map(|h| h.trim().to_string())
                    .filter(|h| !h.is_empty())
            });
        self.branch_cache = Some((name.clone(), Instant::now()));
        name
    }

    /// 2 秒キャッシュ付きで `git -C <ws> status --porcelain=v1` を実行しパースする。
    pub fn refresh_if_stale(&mut self) {
        if let Some(t) = self.last_refresh {
            if t.elapsed() < STATUS_CACHE_TTL {
                return;
            }
        }
        // 失敗時 (git 無し / 非 repo) も時刻は更新し、毎フレーム再実行しない。
        self.last_refresh = Some(Instant::now());
        match self.run_git(&["status", "--porcelain=v1"]) {
            Some(out) => self.status_cache = parse_porcelain_status(&out),
            None => self.status_cache.clear(),
        }
    }

    /// 相対パスのステータス (refresh_if_stale 済み前提、キャッシュから)。
    pub fn file_status(&self, rel_path: &str) -> Option<FileStatus> {
        self.status_cache.get(rel_path).copied()
    }

    /// 相対ディレクトリパス配下の主要なステータスと変更件数
    pub fn dir_status(&self, rel_dir: &str) -> Option<(FileStatus, usize)> {
        let prefix = if rel_dir.is_empty() {
            String::new()
        } else if rel_dir.ends_with('/') {
            rel_dir.to_string()
        } else {
            format!("{rel_dir}/")
        };

        let mut count = 0;
        let mut best: Option<FileStatus> = None;

        for (path, status) in &self.status_cache {
            if prefix.is_empty() || path.starts_with(&prefix) {
                count += 1;
                best = Some(match (best, status) {
                    (None, s) => *s,
                    (Some(FileStatus::Modified), _) => FileStatus::Modified,
                    (_, FileStatus::Modified) => FileStatus::Modified,
                    (Some(FileStatus::Added), _) => FileStatus::Added,
                    (_, FileStatus::Added) => FileStatus::Added,
                    (Some(FileStatus::Untracked), _) => FileStatus::Untracked,
                    (_, FileStatus::Untracked) => FileStatus::Untracked,
                    (Some(s), _) => s,
                });
            }
        }

        best.map(|s| (s, count))
    }

    /// 変更ファイル数 (status のエントリ数)。
    pub fn dirty_count(&self) -> usize {
        self.status_cache.len()
    }

    /// 指定ファイルの 0-based 行番号 → LineMark。
    /// `text_hash` が前回と同一ならキャッシュを返し、git は再実行しない。
    /// 戻りは Arc 共有: キャッシュヒット時は参照カウント増加のみで Vec は複製しない。
    pub fn line_marks(&mut self, rel_path: &str, text_hash: u64) -> Arc<Vec<(usize, LineMark)>> {
        if let Some((hash, marks)) = self.marks_cache.get(rel_path) {
            if *hash == text_hash {
                return Arc::clone(marks);
            }
        }
        let marks = Arc::new(
            self.run_git(&["diff", "--unified=0", "--", rel_path])
                .map(|out| parse_hunk_marks(&out))
                .unwrap_or_default(),
        );
        self.marks_cache
            .insert(rel_path.to_string(), (text_hash, Arc::clone(&marks)));
        marks
    }

    /// `git -C <workspace> <args>` を実行。git 不在・非 repo・失敗時は None。
    fn run_git(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.workspace)
            .args(args)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout).ok()
    }
}

/// マルチルートワークスペース用の Git 束ね。
///
/// ルートそのものではなく **リポジトリのトップレベル**をキーにするため、
/// - ルートが repo のサブディレクトリでも status / diff が正しく引ける
/// - 同一 repo 内の 2 ルートは 1 つの `Git` を共有する(git 実行が二重にならない)
///
/// トップレベル探索 (`rev-parse --show-toplevel`) はルート毎に 1 回だけ行い
/// キャッシュする。status / diff の TTL キャッシュは `Git` 側のものをそのまま使う。
pub struct GitSet {
    roots: Vec<PathBuf>,
    /// repo トップレベル → Git
    repos: HashMap<PathBuf, Git>,
    /// ルート → repo トップレベル (None = 非 repo。再探索しない)
    toplevels: HashMap<PathBuf, Option<PathBuf>>,
}

impl GitSet {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        let mut s = Self {
            roots: Vec::new(),
            repos: HashMap::new(),
            toplevels: HashMap::new(),
        };
        s.set_roots(roots);
        s
    }

    /// ルート一覧を差し替える。既に探索済みのルートのキャッシュは再利用する。
    pub fn set_roots(&mut self, roots: Vec<PathBuf>) {
        self.roots = roots;
        for r in self.roots.clone() {
            self.ensure_repo(&r);
        }
        // どのルートからも到達しなくなった repo を捨てる
        let live: Vec<PathBuf> = self
            .roots
            .iter()
            .filter_map(|r| self.toplevels.get(r).cloned().flatten())
            .collect();
        self.repos.retain(|top, _| live.contains(top));
        self.toplevels.retain(|r, _| self.roots.contains(r));
    }

    /// `root` の repo トップレベルを(未探索なら探索して)確定させる。
    fn ensure_repo(&mut self, root: &Path) -> Option<PathBuf> {
        if let Some(t) = self.toplevels.get(root) {
            return t.clone();
        }
        let top = discover_toplevel(root);
        self.toplevels.insert(root.to_path_buf(), top.clone());
        if let Some(t) = &top {
            self.repos
                .entry(t.clone())
                .or_insert_with(|| Git::new(t.clone()));
        }
        top
    }

    /// `abs` を含むルート(最長一致)。
    fn root_for(&self, abs: &Path) -> Option<&Path> {
        crate::file_tree::root_for(&self.roots, abs)
    }

    /// `abs` → (repo トップレベル, repo からの相対パス)。
    fn resolve(&self, abs: &Path) -> Option<(PathBuf, String)> {
        let root = self.root_for(abs)?;
        let top = self.toplevels.get(root)?.clone()?;
        let rel = abs.strip_prefix(&top).ok()?;
        Some((top, rel.to_string_lossy().to_string()))
    }

    /// 全 repo の status を TTL 付きで更新する。
    pub fn refresh_if_stale(&mut self) {
        for g in self.repos.values_mut() {
            g.refresh_if_stale();
        }
    }

    /// 絶対パスのステータス (refresh_if_stale 済み前提)。
    pub fn file_status(&self, abs: &Path) -> Option<FileStatus> {
        let (top, rel) = self.resolve(abs)?;
        self.repos.get(&top)?.file_status(&rel)
    }

    /// 絶対パスのディレクトリのステータスと変更件数 (refresh_if_stale 済み前提)。
    pub fn dir_status(&self, abs: &Path) -> Option<(FileStatus, usize)> {
        let (top, rel) = self.resolve(abs)?;
        self.repos.get(&top)?.dir_status(&rel)
    }

    /// 全 repo の変更ファイル数の合計。
    pub fn dirty_count(&self) -> usize {
        self.repos.values().map(|g| g.dirty_count()).sum()
    }

    /// 絶対パスの行マーク。repo 外なら空。
    pub fn line_marks(&mut self, abs: &Path, text_hash: u64) -> Arc<Vec<(usize, LineMark)>> {
        let Some((top, rel)) = self.resolve(abs) else {
            return empty_line_marks();
        };
        match self.repos.get_mut(&top) {
            Some(g) => g.line_marks(&rel, text_hash),
            None => empty_line_marks(),
        }
    }

    /// primary ルートが属する repo のブランチ名。
    pub fn branch(&mut self) -> Option<String> {
        let top = self.roots.first().and_then(|r| self.toplevels.get(r))?.clone()?;
        self.repos.get_mut(&top)?.branch()
    }

    /// repo の数(ステータスバー等で「複数リポジトリ」表示に使う)。
    pub fn repo_count(&self) -> usize {
        self.repos.len()
    }
}

/// `status --porcelain=v1` の出力をパースする。
fn parse_porcelain_status(output: &str) -> HashMap<String, FileStatus> {
    let mut map = HashMap::new();
    for line in output.lines() {
        // 形式: "XY <path>" / "XY <orig> -> <path>" (XY は ASCII 2 文字 + 空白 1)
        if line.len() < 4 || !line.is_char_boundary(2) {
            continue;
        }
        let code = &line[..2];
        let mut path = line[3..].trim();
        if let Some((_, new_path)) = path.split_once(" -> ") {
            path = new_path;
        }
        let path = path.trim_matches('"');
        let status = if code == "??" {
            FileStatus::Untracked
        } else if code.contains('R') || code.contains('C') {
            FileStatus::Renamed
        } else if code.contains('A') {
            FileStatus::Added
        } else if code.contains('D') {
            FileStatus::Deleted
        } else if code.contains('M') || code.contains('T') || code.contains('U') {
            FileStatus::Modified
        } else {
            continue;
        };
        map.insert(path.to_string(), status);
    }
    map
}

/// `+c,d` / `-a,b` / `+c` (カウント省略 = 1) を (start, count) にパースする。
fn parse_range(token: &str) -> Option<(usize, usize)> {
    let body = token
        .strip_prefix('+')
        .or_else(|| token.strip_prefix('-'))?;
    let mut parts = body.splitn(2, ',');
    let start: usize = parts.next()?.trim().parse().ok()?;
    let count: usize = match parts.next() {
        Some(c) => c.trim().parse().ok()?,
        None => 1,
    };
    Some((start, count))
}

/// diff 出力中のハンクヘッダ `@@ -a,b +c,d @@` をパースし、
/// 0-based 行番号 → LineMark の一覧を返す純関数。
///
/// - b == 0            → 新ファイル側 c..c+d 行が Added
/// - b > 0 && d > 0    → 新ファイル側 c..c+d 行が Modified
/// - d == 0 (削除のみ) → マークなし
///
/// diff の +c は 1-based なので 0-based へ正規化する。b / d 省略時は 1。
pub fn parse_hunk_marks(diff_output: &str) -> Vec<(usize, LineMark)> {
    let mut marks = Vec::new();
    for line in diff_output.lines() {
        if !line.starts_with("@@") {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let _at = tokens.next(); // "@@"
        let (old_tok, new_tok) = match (tokens.next(), tokens.next()) {
            (Some(o), Some(n)) if o.starts_with('-') && n.starts_with('+') => (o, n),
            _ => continue,
        };
        let Some((_a, b)) = parse_range(old_tok) else {
            continue;
        };
        let Some((c, d)) = parse_range(new_tok) else {
            continue;
        };
        if d == 0 {
            // 削除のみ: 新ファイル側に対応行が無いためマークしない。
            continue;
        }
        let mark = if b == 0 {
            LineMark::Added
        } else {
            LineMark::Modified
        };
        let start = c.saturating_sub(1); // 1-based → 0-based
        for i in 0..d {
            marks.push((start + i, mark));
        }
    }
    marks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_only_hunk() {
        // 旧 10 行目の後に 3 行追加 → 新ファイル 1-based 11..13 → 0-based 10..12
        let out = parse_hunk_marks("@@ -10,0 +11,3 @@");
        assert_eq!(
            out,
            vec![
                (10, LineMark::Added),
                (11, LineMark::Added),
                (12, LineMark::Added),
            ]
        );
    }

    #[test]
    fn modified_hunk() {
        // 2 行変更 → 1-based 5..6 → 0-based 4..5
        let out = parse_hunk_marks("@@ -5,2 +5,2 @@ fn main()");
        assert_eq!(out, vec![(4, LineMark::Modified), (5, LineMark::Modified)]);
    }

    #[test]
    fn deleted_only_hunk_yields_no_marks() {
        // d == 0 (削除のみ) はマークなし
        let out = parse_hunk_marks("@@ -7,3 +6,0 @@");
        assert!(out.is_empty());
    }

    #[test]
    fn multiple_hunks_with_diff_noise() {
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1234567..89abcde 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,1 +1,1 @@
-old line
+new line
@@ -4,0 +5,2 @@
+added one
+added two
@@ -20,2 +21,0 @@
-gone
-gone too
";
        let out = parse_hunk_marks(diff);
        assert_eq!(
            out,
            vec![
                (0, LineMark::Modified),
                (4, LineMark::Added),
                (5, LineMark::Added),
            ]
        );
    }

    #[test]
    fn omitted_counts_default_to_one() {
        // "@@ -3 +3 @@": b, d とも省略 = 1 → 0-based 2 が Modified
        let out = parse_hunk_marks("@@ -3 +3 @@");
        assert_eq!(out, vec![(2, LineMark::Modified)]);
        // "@@ -0,0 +1 @@": b == 0, d 省略 = 1 → 0-based 0 が Added
        let out = parse_hunk_marks("@@ -0,0 +1 @@");
        assert_eq!(out, vec![(0, LineMark::Added)]);
    }

    #[test]
    fn non_hunk_lines_and_garbage_ignored() {
        assert!(parse_hunk_marks("").is_empty());
        assert!(parse_hunk_marks("hello world\n+not a hunk\n@@ broken @@").is_empty());
    }

    /// `git init` した使い捨てリポジトリを作る。git が無い環境では None。
    fn temp_repo(tag: &str) -> Option<PathBuf> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "zaivern-git-test-{}-{}-{}-{}",
            tag,
            std::process::id(),
            nanos,
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("create temp repo dir");
        let ok = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init", "--quiet"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            std::fs::remove_dir_all(&dir).ok();
            return None;
        }
        Some(dir)
    }

    #[test]
    fn toplevel_discovery_from_subdirectory() {
        let Some(repo) = temp_repo("toplevel") else {
            return; // git が無い環境ではスキップ
        };
        let sub = repo.join("crates").join("inner");
        std::fs::create_dir_all(&sub).expect("mkdir sub");

        let canon_repo = repo.canonicalize().expect("canonicalize repo");
        assert_eq!(
            discover_toplevel(&sub).map(|p| p.canonicalize().unwrap()),
            Some(canon_repo.clone()),
            "サブディレクトリからでも repo トップレベルが取れる",
        );

        // サブディレクトリをルートにしても、repo トップレベル基準で解決される
        let mut set = GitSet::new(vec![sub.canonicalize().expect("canonicalize sub")]);
        assert_eq!(set.repo_count(), 1);
        let (top, rel) = set
            .resolve(&sub.canonicalize().unwrap().join("lib.rs"))
            .expect("resolve should find the repo");
        assert_eq!(top.canonicalize().unwrap(), canon_repo);
        assert_eq!(rel, "crates/inner/lib.rs", "repo 相対パスになる");

        // ブランチ名は rev-parse 経由で取れる(worktree/submodule 対応)
        assert!(set.branch().is_some(), "初期化直後でもブランチ名が取れる");

        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn two_roots_in_same_repo_share_one_git() {
        let Some(repo) = temp_repo("shared") else {
            return;
        };
        let a = repo.join("a");
        let b = repo.join("b");
        std::fs::create_dir_all(&a).expect("mkdir a");
        std::fs::create_dir_all(&b).expect("mkdir b");

        let set = GitSet::new(vec![
            a.canonicalize().expect("canon a"),
            b.canonicalize().expect("canon b"),
        ]);
        assert_eq!(set.repo_count(), 1, "同一 repo の 2 ルートは Git を共有する");

        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn non_repo_root_yields_no_repo() {
        let dir = std::env::temp_dir().join(format!(
            "zaivern-git-norepo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        // /tmp 配下が repo でない前提が崩れる環境もあるため、結果は緩く検証する
        let mut set = GitSet::new(vec![dir.canonicalize().expect("canon")]);
        if set.repo_count() == 0 {
            assert!(set.branch().is_none());
            assert_eq!(set.dirty_count(), 0);
            assert!(set.line_marks(&dir.join("x.rs"), 0).is_empty());
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_porcelain_status_variants() {
        // 注意: 行頭の空白が porcelain フォーマットの一部なので \n エスケープで記述
        let out = " M src/app.rs\nA  src/new.rs\n?? notes.txt\n D gone.rs\nR  old.rs -> renamed.rs\n";
        let map = parse_porcelain_status(out);
        assert_eq!(map.get("src/app.rs"), Some(&FileStatus::Modified));
        assert_eq!(map.get("src/new.rs"), Some(&FileStatus::Added));
        assert_eq!(map.get("notes.txt"), Some(&FileStatus::Untracked));
        assert_eq!(map.get("gone.rs"), Some(&FileStatus::Deleted));
        assert_eq!(map.get("renamed.rs"), Some(&FileStatus::Renamed));
        assert_eq!(map.len(), 5);
    }
}
