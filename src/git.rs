//! Git 連携モジュール。
//!
//! `git` CLI (std::process::Command) を用いてワークスペースのステータスと
//! 行単位の diff マークを取得する。git が無い場合や workspace が
//! git リポジトリでない場合は、すべて空 / None を返す。

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// status --porcelain=v1 から得たファイル単位の実効ステータス。
/// index 列と worktree 列を 1 つに畳んだもの (VS Code のツリー装飾と同じ粒度)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileStatus {
    Modified,
    Added,
    Untracked,
    Deleted,
    Renamed,
    /// マージコンフリクト (UU / AA / DD など)。
    Conflicted,
}

/// エディタのガター等に表示する行マーク。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineMark {
    Added,
    Modified,
}

const STATUS_CACHE_TTL: Duration = Duration::from_secs(2);
const BRANCH_CACHE_TTL: Duration = Duration::from_secs(3);

/// 巨大リポジトリ対策: porcelain 出力のパース上限 (エントリ数)。
/// 超過分は黙って捨て、取り込めた分だけ色付けする (騒がしいログは出さない)。
const MAX_STATUS_ENTRIES: usize = 10_000;

/// バックグラウンドの status スキャン 1 回分の結果。
/// UI スレッドへはチャネルでこの塊ごと渡し、届いた時点でまとめて
/// 差し替える (フレーム途中で新旧が混ざらないアトミックスワップ)。
struct StatusSnapshot {
    /// 相対パス → 実効ステータス。
    files: HashMap<String, FileStatus>,
    /// 相対ディレクトリ ("" = repo ルート) → (代表ステータス, 配下の変更件数)。
    /// 変更ファイルの祖先ディレクトリを事前計算し、描画時の全走査を避ける。
    dirs: HashMap<String, (FileStatus, usize)>,
    /// 相対ディレクトリ → 削除済みファイル名 (幽霊行表示用、ソート済み)。
    deleted_by_dir: HashMap<String, Vec<String>>,
}

/// 共有の空行マーク。repo 外・非 repo のバッファで毎フレーム返す値なので、
/// その都度 `Arc::new(Vec::new())` でアロケしないよう 1 つを使い回す。
pub(crate) fn empty_line_marks() -> Arc<Vec<(usize, LineMark)>> {
    static EMPTY: OnceLock<Arc<Vec<(usize, LineMark)>>> = OnceLock::new();
    Arc::clone(EMPTY.get_or_init(|| Arc::new(Vec::new())))
}

/// `dir` が属する git リポジトリのトップレベルを返す(非 repo / git 不在なら None)。
/// ルートがリポジトリのサブディレクトリでも正しいトップレベルが得られる。
pub fn discover_toplevel(dir: &Path) -> Option<PathBuf> {
    let out = crate::procx::hidden_command("git")
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
    // アプリ内の正規形 (pathx::canonical = plain 形) に揃える。Windows では
    // git が `C:/Users/RUNNER~1/...` (スラッシュ + 8.3 短縮名) を返すことが
    // あり、canonicalize 済みのルートと strip_prefix で突き合わせられない。
    Some(crate::pathx::canonical(&PathBuf::from(s)))
}

/// marks_cache の値: (text_hash, 取得時刻, 行マーク)。
/// Arc 共有: キャッシュヒット時に Vec を複製しない。
/// 取得時刻はタイプ中のデバウンス用 (毎キーストロークで git diff を
/// 同期起動すると UI スレッドがヒッチするため)。
type MarksEntry = (u64, Instant, Arc<Vec<(usize, LineMark)>>);

/// タイプ中に行マークを取り直す最短間隔。
const MARKS_DEBOUNCE: Duration = Duration::from_millis(400);

pub struct Git {
    workspace: PathBuf,
    /// 相対パス → ステータス (status --porcelain=v1 -z のパース結果)。
    status_cache: HashMap<String, FileStatus>,
    /// 相対ディレクトリ → (代表ステータス, 件数) の事前集計。
    dir_cache: HashMap<String, (FileStatus, usize)>,
    /// 相対ディレクトリ → 削除済みファイル名 (幽霊行用)。
    deleted_cache: HashMap<String, Vec<String>>,
    /// 最後に status スキャンを開始した時刻。None なら未実行。
    last_refresh: Option<Instant>,
    /// 実行中のバックグラウンドスキャン (git_panel.rs の poll 方式に倣う)。
    /// None ペイロード = git 不在 / 非 repo。
    pending: Option<Receiver<Option<StatusSnapshot>>>,
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
            dir_cache: HashMap::new(),
            deleted_cache: HashMap::new(),
            last_refresh: None,
            pending: None,
            marks_cache: HashMap::new(),
            branch_cache: None,
        }
    }

    pub fn set_workspace(&mut self, ws: PathBuf) {
        if self.workspace != ws {
            self.workspace = ws;
            self.status_cache.clear();
            self.dir_cache.clear();
            self.deleted_cache.clear();
            self.marks_cache.clear();
            self.last_refresh = None;
            self.pending = None;
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

    /// 2 秒 TTL で status スキャンを回す (呼び出しは毎フレームでも安全)。
    pub fn refresh_if_stale(&mut self) {
        self.refresh(false);
    }

    /// status の更新。git は **UI スレッドでは実行せず**、バックグラウンド
    /// スレッド + チャネルで受け取る (voice.rs / git_panel.rs と同じ方式)。
    /// `force` はウィンドウフォーカス復帰など「今すぐ見たい」契機で TTL を無視する。
    fn refresh(&mut self, force: bool) {
        // 1) 完了したスキャンがあれば取り込む (アトミックスワップ)
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(snap) => {
                    self.apply_scan(snap);
                    self.pending = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        // 2) TTL 切れ (または強制) なら新しいスキャンを開始
        if self.pending.is_some() {
            return;
        }
        if !force {
            if let Some(t) = self.last_refresh {
                if t.elapsed() < STATUS_CACHE_TTL {
                    return;
                }
            }
        }
        // 失敗時 (git 無し / 非 repo) も時刻は更新し、毎フレーム再起動しない。
        self.last_refresh = Some(Instant::now());
        let (tx, rx) = mpsc::channel();
        let ws = self.workspace.clone();
        let spawned = std::thread::Builder::new()
            .name("zv-git-status".into())
            .spawn(move || {
                let _ = tx.send(scan_status(&ws));
            });
        if spawned.is_ok() {
            self.pending = Some(rx);
        }
    }

    /// スキャン結果の取り込み。None = git 失敗 → 全て空にする。
    fn apply_scan(&mut self, snap: Option<StatusSnapshot>) {
        match snap {
            Some(snap) => {
                // コミット/チェックアウト/ステージ等で状態が変わったら、
                // 本文ハッシュが同じでも行マークは古いので取り直させる
                if snap.files != self.status_cache {
                    self.marks_cache.clear();
                }
                self.status_cache = snap.files;
                self.dir_cache = snap.dirs;
                self.deleted_cache = snap.deleted_by_dir;
            }
            None => {
                self.status_cache.clear();
                self.dir_cache.clear();
                self.deleted_cache.clear();
                self.marks_cache.clear();
            }
        }
    }

    /// 相対パスのステータス (refresh_if_stale 済み前提、キャッシュから)。
    pub fn file_status(&self, rel_path: &str) -> Option<FileStatus> {
        self.status_cache.get(rel_path).copied()
    }

    /// 相対ディレクトリパス配下の代表ステータスと変更件数。
    /// スキャン時に事前集計した dir_cache の O(1) 参照 (描画毎の全走査をしない)。
    pub fn dir_status(&self, rel_dir: &str) -> Option<(FileStatus, usize)> {
        let key = rel_dir.trim_end_matches('/');
        self.dir_cache.get(key).copied()
    }

    /// 相対ディレクトリ直下の「git 上は削除済み」ファイル名 (幽霊行表示用)。
    pub fn deleted_names_in(&self, rel_dir: &str) -> &[String] {
        let key = rel_dir.trim_end_matches('/');
        self.deleted_cache.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    /// 変更ファイル数 (status のエントリ数)。
    pub fn dirty_count(&self) -> usize {
        self.status_cache.len()
    }

    /// 指定ファイルの 0-based 行番号 → LineMark。
    /// `text_hash` が前回と同一ならキャッシュを返し、git は再実行しない。
    /// 戻りは Arc 共有: キャッシュヒット時は参照カウント増加のみで Vec は複製しない。
    pub fn line_marks(&mut self, rel_path: &str, text_hash: u64) -> Arc<Vec<(usize, LineMark)>> {
        if let Some((hash, at, marks)) = self.marks_cache.get(rel_path) {
            // タイプ中はデバウンス: ハッシュが変わっていても直近に取った
            // マークをそのまま返し、git diff の同期起動を 400ms に 1 回へ
            // 抑える (キーストロークごとのプロセス起動はヒッチになる)。
            if *hash == text_hash || at.elapsed() < MARKS_DEBOUNCE {
                return Arc::clone(marks);
            }
        }
        let marks = Arc::new(
            self.run_git(&["diff", "--unified=0", "--", rel_path])
                .map(|out| parse_hunk_marks(&out))
                .unwrap_or_default(),
        );
        self.marks_cache
            .insert(rel_path.to_string(), (text_hash, Instant::now(), Arc::clone(&marks)));
        marks
    }

    /// `git -C <workspace> <args>` を実行。git 不在・非 repo・失敗時は None。
    fn run_git(&self, args: &[&str]) -> Option<String> {
        let out = crate::procx::hidden_command("git")
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
    /// 次回 refresh_if_stale で TTL を無視する予約 (ウィンドウフォーカス復帰など)。
    /// `&self` しか持てない描画側 (file_tree) からも要求できるよう Atomic。
    force_refresh: AtomicBool,
}

impl GitSet {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        let mut s = Self {
            roots: Vec::new(),
            repos: HashMap::new(),
            toplevels: HashMap::new(),
            force_refresh: AtomicBool::new(false),
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
        let mut rel = rel.to_string_lossy().to_string();
        // git の status/diff はパスを / 区切りで報告する。照合キーになる
        // 相対パスは Windows でも / に正規化して持つ。
        if cfg!(windows) {
            rel = rel.replace('\\', "/");
        }
        Some((top, rel))
    }

    /// 全 repo の status を TTL 付きで更新する (実行はバックグラウンド)。
    pub fn refresh_if_stale(&mut self) {
        let force = self.force_refresh.swap(false, Ordering::Relaxed);
        for g in self.repos.values_mut() {
            g.refresh(force);
        }
    }

    /// 次回の refresh_if_stale で TTL を無視して即スキャンさせる。
    /// ウィンドウフォーカス復帰・保存直後など「外で変わったかも」な契機用。
    pub fn request_refresh(&self) {
        self.force_refresh.store(true, Ordering::Relaxed);
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

    /// 絶対パスのディレクトリ直下の「git 上は削除済み」ファイル名 (幽霊行用)。
    pub fn deleted_names_in(&self, abs_dir: &Path) -> &[String] {
        let Some((top, rel)) = self.resolve(abs_dir) else {
            return &[];
        };
        match self.repos.get(&top) {
            Some(g) => g.deleted_names_in(&rel),
            None => &[],
        }
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

/// index 列 `x` + worktree 列 `y` (porcelain v1 の XY) を 1 つの実効ステータスへ畳む。
/// None = ツリーに出さない (無視ファイル `!!`・未変更)。
///
/// 優先順位 (VS Code のツリー装飾に合わせる):
/// 未追跡 > コンフリクト > 削除 (worktree から消えている方が優先) >
/// リネーム/コピー > 追加 > 変更。
/// 例: "AM"=Added, "AD"=Deleted, "RM"=Renamed, "UU"/"AA"/"DD"=Conflicted。
fn effective_status(x: char, y: char) -> Option<FileStatus> {
    if x == '?' || y == '?' {
        return Some(FileStatus::Untracked);
    }
    if x == '!' || y == '!' {
        return None; // ignored はツリーに色付けしない
    }
    if x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D') {
        return Some(FileStatus::Conflicted);
    }
    if x == 'D' || y == 'D' {
        return Some(FileStatus::Deleted);
    }
    if x == 'R' || y == 'R' || x == 'C' || y == 'C' {
        return Some(FileStatus::Renamed);
    }
    if x == 'A' || y == 'A' {
        return Some(FileStatus::Added);
    }
    if x == 'M' || y == 'M' || x == 'T' || y == 'T' {
        return Some(FileStatus::Modified);
    }
    None
}

/// `status --porcelain=v1 -z` の出力をパースする。
///
/// NUL 区切りなのでパスの引用・エスケープが無く、リネームは
/// `XY <new>\0<old>\0` の 2 トークン組で届く (旧パスは読み捨てる)。
/// `cap` 件を超えたら打ち切り、取り込めた分だけ返す (巨大 repo の劣化運転)。
fn parse_porcelain_z(output: &str, cap: usize) -> HashMap<String, FileStatus> {
    let mut map = HashMap::new();
    let mut tokens = output.split('\0');
    while let Some(tok) = tokens.next() {
        if map.len() >= cap {
            break;
        }
        let mut chars = tok.chars();
        let (Some(x), Some(y), Some(' ')) = (chars.next(), chars.next(), chars.next()) else {
            continue; // 末尾の空トークンや壊れたエントリ
        };
        let path = chars.as_str();
        if path.is_empty() {
            continue;
        }
        // リネーム/コピーは次のトークンが旧パス。ここで消費して読み捨てる。
        if x == 'R' || x == 'C' || y == 'R' || y == 'C' {
            let _ = tokens.next();
        }
        if let Some(status) = effective_status(x, y) {
            map.insert(path.to_string(), status);
        }
    }
    map
}

/// 変更ファイル群から祖先ディレクトリの集計を導出する。
///
/// キーは相対ディレクトリ ("" = repo ルート)。値は (代表ステータス, 件数)。
/// VS Code 同様、配下が単一種ならその色、混在なら Modified の色調で塗る。
fn derive_dir_status(files: &HashMap<String, FileStatus>) -> HashMap<String, (FileStatus, usize)> {
    let mut dirs: HashMap<String, (FileStatus, usize)> = HashMap::new();
    for (path, st) in files {
        let mut bump = |dir: &str| {
            let e = dirs.entry(dir.to_string()).or_insert((*st, 0));
            e.1 += 1;
            if e.0 != *st {
                e.0 = FileStatus::Modified;
            }
        };
        bump("");
        let mut idx = 0;
        while let Some(pos) = path[idx..].find('/') {
            idx += pos;
            bump(&path[..idx]);
            idx += 1;
        }
    }
    dirs
}

/// 削除済みファイルを「親ディレクトリ → ファイル名一覧」に整理する (幽霊行用)。
fn derive_deleted_by_dir(files: &HashMap<String, FileStatus>) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for (path, st) in files {
        if *st != FileStatus::Deleted {
            continue;
        }
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
        map.entry(dir.to_string()).or_default().push(name.to_string());
    }
    for names in map.values_mut() {
        names.sort();
    }
    map
}

/// `git -C <ws> status --porcelain=v1 -z` を実行し、スナップショットを組み立てる。
/// バックグラウンドスレッドから呼ばれる (UI スレッドでは呼ばない)。
/// git 不在・非 repo・失敗時は None。
fn scan_status(workspace: &Path) -> Option<StatusSnapshot> {
    let out = crate::procx::hidden_command("git")
        .arg("-C")
        .arg(workspace)
        .args(["status", "--porcelain=v1", "-z"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let files = parse_porcelain_z(&text, MAX_STATUS_ENTRIES);
    let dirs = derive_dir_status(&files);
    let deleted_by_dir = derive_deleted_by_dir(&files);
    Some(StatusSnapshot {
        files,
        dirs,
        deleted_by_dir,
    })
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
    use std::process::Command;

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

        // アプリの正規形 (pathx::canonical = plain 形) で統一する。素の
        // canonicalize は Windows で `\\?\` verbatim を返し、製品コードが
        // 持ち回る形 (normalize_roots も pathx::canonical) と食い違う。
        let canon_repo = crate::pathx::canonical(&repo);
        assert_eq!(
            discover_toplevel(&sub).map(|p| crate::pathx::canonical(&p)),
            Some(canon_repo.clone()),
            "サブディレクトリからでも repo トップレベルが取れる",
        );

        // サブディレクトリをルートにしても、repo トップレベル基準で解決される
        let mut set = GitSet::new(vec![crate::pathx::canonical(&sub)]);
        assert_eq!(set.repo_count(), 1);
        let (top, rel) = set
            .resolve(&crate::pathx::canonical(&sub).join("lib.rs"))
            .expect("resolve should find the repo");
        assert_eq!(crate::pathx::canonical(&top), canon_repo);
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
    fn parse_porcelain_z_variants() {
        // 注意: XY 2 文字の空白も porcelain フォーマットの一部。
        // -z 形式: NUL 区切り、リネームは "R  new\0old" の 2 トークン組。
        let out = " M src/app.rs\0A  src/new.rs\0?? notes.txt\0 D gone.rs\0\
                   R  renamed.rs\0old.rs\0UU conflict.rs\0AM both.rs\0MD gone2.rs\0\
                   !! target/ignored.rs\0";
        let map = parse_porcelain_z(out, MAX_STATUS_ENTRIES);
        assert_eq!(map.get("src/app.rs"), Some(&FileStatus::Modified));
        assert_eq!(map.get("src/new.rs"), Some(&FileStatus::Added));
        assert_eq!(map.get("notes.txt"), Some(&FileStatus::Untracked));
        assert_eq!(map.get("gone.rs"), Some(&FileStatus::Deleted));
        assert_eq!(map.get("renamed.rs"), Some(&FileStatus::Renamed));
        assert!(!map.contains_key("old.rs"), "リネーム旧パスは登録しない");
        assert_eq!(map.get("conflict.rs"), Some(&FileStatus::Conflicted));
        assert_eq!(map.get("both.rs"), Some(&FileStatus::Added));
        assert_eq!(map.get("gone2.rs"), Some(&FileStatus::Deleted));
        assert!(!map.contains_key("target/ignored.rs"), "ignored は除外");
        assert_eq!(map.len(), 8);
    }

    #[test]
    fn parse_porcelain_z_handles_spaces_and_cap() {
        // NUL 区切りなのでスペース入りパスも引用なしでそのまま届く
        let out = "?? my notes.txt\0 M dir with space/a.rs\0?? c.rs\0";
        let map = parse_porcelain_z(out, MAX_STATUS_ENTRIES);
        assert_eq!(map.get("my notes.txt"), Some(&FileStatus::Untracked));
        assert_eq!(map.get("dir with space/a.rs"), Some(&FileStatus::Modified));

        // cap 超過は打ち切り、取り込めた分だけ返す (劣化運転)
        let capped = parse_porcelain_z(out, 2);
        assert_eq!(capped.len(), 2);
    }

    #[test]
    fn effective_status_precedence_table() {
        // (index 列, worktree 列) → 実効ステータス
        let table: &[(char, char, Option<FileStatus>)] = &[
            ('M', ' ', Some(FileStatus::Modified)),
            (' ', 'M', Some(FileStatus::Modified)),
            ('M', 'M', Some(FileStatus::Modified)),
            (' ', 'T', Some(FileStatus::Modified)),
            ('A', ' ', Some(FileStatus::Added)),
            ('A', 'M', Some(FileStatus::Added)), // 追加後に編集 → VS Code は A
            ('A', 'D', Some(FileStatus::Deleted)), // 追加後に削除 → worktree に無い
            ('D', ' ', Some(FileStatus::Deleted)),
            (' ', 'D', Some(FileStatus::Deleted)),
            ('M', 'D', Some(FileStatus::Deleted)),
            ('R', ' ', Some(FileStatus::Renamed)),
            ('R', 'M', Some(FileStatus::Renamed)),
            ('R', 'D', Some(FileStatus::Deleted)), // リネーム後に削除
            ('C', ' ', Some(FileStatus::Renamed)),
            ('?', '?', Some(FileStatus::Untracked)),
            ('!', '!', None), // ignored
            (' ', ' ', None),
            // コンフリクト全種 (git status 仕様の unmerged 組み合わせ)
            ('U', 'U', Some(FileStatus::Conflicted)),
            ('A', 'A', Some(FileStatus::Conflicted)),
            ('D', 'D', Some(FileStatus::Conflicted)),
            ('A', 'U', Some(FileStatus::Conflicted)),
            ('U', 'A', Some(FileStatus::Conflicted)),
            ('D', 'U', Some(FileStatus::Conflicted)),
            ('U', 'D', Some(FileStatus::Conflicted)),
        ];
        for (x, y, want) in table {
            assert_eq!(
                effective_status(*x, *y),
                *want,
                "XY = {x:?}{y:?} の実効ステータス"
            );
        }
    }

    #[test]
    fn derive_dir_status_ancestors_and_mix() {
        let mut files = HashMap::new();
        files.insert("a/b/c.rs".to_string(), FileStatus::Modified);
        files.insert("a/b/d.rs".to_string(), FileStatus::Added);
        files.insert("a/x.rs".to_string(), FileStatus::Added);
        files.insert("top.rs".to_string(), FileStatus::Added);
        let dirs = derive_dir_status(&files);
        // 混在 (M + A) は Modified の色調に寄せる (VS Code 挙動)
        assert_eq!(dirs.get("a/b"), Some(&(FileStatus::Modified, 2)));
        assert_eq!(dirs.get("a"), Some(&(FileStatus::Modified, 3)));
        assert_eq!(dirs.get(""), Some(&(FileStatus::Modified, 4)));
        assert!(!dirs.contains_key("a/b/c.rs"), "ファイル自身は含まない");

        // 単一種ならその色のまま
        let mut only_added = HashMap::new();
        only_added.insert("pkg/one.rs".to_string(), FileStatus::Added);
        only_added.insert("pkg/two.rs".to_string(), FileStatus::Added);
        let dirs = derive_dir_status(&only_added);
        assert_eq!(dirs.get("pkg"), Some(&(FileStatus::Added, 2)));
        assert_eq!(dirs.get(""), Some(&(FileStatus::Added, 2)));
    }

    #[test]
    fn derive_deleted_by_dir_groups_and_sorts() {
        let mut files = HashMap::new();
        files.insert("a/z.rs".to_string(), FileStatus::Modified);
        files.insert("a/b.rs".to_string(), FileStatus::Deleted);
        files.insert("a/a.rs".to_string(), FileStatus::Deleted);
        files.insert("root.rs".to_string(), FileStatus::Deleted);
        let map = derive_deleted_by_dir(&files);
        assert_eq!(
            map.get("a"),
            Some(&vec!["a.rs".to_string(), "b.rs".to_string()]),
            "名前順にソート済み"
        );
        assert_eq!(map.get(""), Some(&vec!["root.rs".to_string()]));
        assert_eq!(map.len(), 2);
    }

    /// 実 repo フィクスチャ: git init → 作成/変更/削除/リネームして
    /// scan_status のスナップショット全体を検証する。
    #[test]
    fn scan_status_real_fixture() {
        let Some(repo) = temp_repo("scanfix") else {
            return; // git が無い環境ではスキップ
        };
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=zv", "-c", "user.email=zv@example.com"])
                .args(args)
                .output()
                .expect("git 実行")
        };
        let sub = repo.join("src");
        std::fs::create_dir_all(&sub).expect("mkdir src");
        std::fs::write(sub.join("keep.rs"), "fn main() {}\n").expect("write keep");
        std::fs::write(sub.join("gone.rs"), "old\n").expect("write gone");
        std::fs::write(repo.join("old_name.rs"), "same body\n").expect("write old");
        run(&["add", "-A"]);
        run(&["commit", "-m", "init", "--quiet"]);

        // 変更 / 削除 / リネーム / 未追跡 をそれぞれ 1 つずつ作る
        std::fs::write(sub.join("keep.rs"), "fn main() { changed(); }\n").expect("modify");
        std::fs::remove_file(sub.join("gone.rs")).expect("delete");
        run(&["mv", "old_name.rs", "new_name.rs"]);
        std::fs::write(repo.join("fresh.txt"), "hi\n").expect("write fresh");

        let snap = scan_status(&repo).expect("scan_status");
        assert_eq!(snap.files.get("src/keep.rs"), Some(&FileStatus::Modified));
        assert_eq!(snap.files.get("src/gone.rs"), Some(&FileStatus::Deleted));
        assert_eq!(snap.files.get("new_name.rs"), Some(&FileStatus::Renamed));
        assert!(!snap.files.contains_key("old_name.rs"));
        assert_eq!(snap.files.get("fresh.txt"), Some(&FileStatus::Untracked));

        // 祖先ディレクトリ集計: src は M+D 混在 → Modified 扱い、2 件
        assert_eq!(snap.dirs.get("src"), Some(&(FileStatus::Modified, 2)));
        assert_eq!(snap.dirs.get("").map(|(_, n)| *n), Some(4));

        // 幽霊行: src 直下に gone.rs
        assert_eq!(
            snap.deleted_by_dir.get("src"),
            Some(&vec!["gone.rs".to_string()])
        );

        std::fs::remove_dir_all(&repo).ok();
    }

    /// バックグラウンドスキャン + チャネル取り込みの結線検証。
    /// refresh_if_stale を回し続け、スナップショットが届くまで待つ。
    #[test]
    fn background_refresh_lands_via_channel() {
        let Some(repo) = temp_repo("bgscan") else {
            return;
        };
        std::fs::write(repo.join("fresh.txt"), "hi\n").expect("write");

        let mut g = Git::new(repo.clone());
        g.refresh_if_stale(); // スキャン開始 (この時点では空のまま)
        let mut landed = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(50));
            g.refresh(true); // ポーリング (force でも pending 中は再起動しない)
            if g.file_status("fresh.txt") == Some(FileStatus::Untracked) {
                landed = true;
                break;
            }
        }
        assert!(landed, "バックグラウンドスキャンの結果が届く");
        assert_eq!(g.dir_status(""), Some((FileStatus::Untracked, 1)));

        std::fs::remove_dir_all(&repo).ok();
    }
}
