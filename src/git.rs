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

use eframe::egui;

use crate::i18n::{tr, trf};

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

    /// ブランチ名の TTL キャッシュを捨てる (checkout 直後など、
    /// 次の描画で必ず新しい名前を出したいとき)。
    pub fn invalidate_branch(&mut self) {
        self.branch_cache = None;
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
    ///
    /// サブモジュールは外側 repo の status に「ディレクトリ 1 エントリ」
    /// (` M sub`) として出るため dir_cache には入らない。その場合だけ
    /// ファイル側のエントリへフォールバックし、フォルダ行にも色を出す。
    /// 中身のパス (`sub/x.rs`) は外側の status に一切現れないので、
    /// 内側リポジトリの変更が外側に誤って染み出すことはない。
    pub fn dir_status(&self, rel_dir: &str) -> Option<(FileStatus, usize)> {
        let key = rel_dir.trim_end_matches('/');
        if let Some(v) = self.dir_cache.get(key) {
            return Some(*v);
        }
        if key.is_empty() {
            return None;
        }
        self.status_cache.get(key).map(|s| (*s, 1))
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

    /// 全 repo のブランチ名キャッシュを捨てる (checkout 直後など)。
    pub fn invalidate_branch(&mut self) {
        for g in self.repos.values_mut() {
            g.invalidate_branch();
        }
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

/// `status --porcelain=v1 -z` のパース結果。
#[derive(Default, Debug, PartialEq, Eq)]
struct ParsedStatus {
    /// 相対パス → 実効ステータス (ツリーに実体がある側)。
    files: HashMap<String, FileStatus>,
    /// リネーム/コピーの (旧パス, 新パス)。旧パスはツリーに実体が無いが、
    /// 「ここから何かが動いた」を見せるため旧側の親フォルダも色付ける材料にする。
    renames: Vec<(String, String)>,
}

/// 祖先ディレクトリ集計を辿る最大深さ (病的に深いパスでのメモリ暴走止め)。
/// file_tree 側の描画上限 (depth 24) より十分深い。
const MAX_DIR_DEPTH: usize = 64;

/// `path` の祖先ディレクトリを浅い順に返す ("" = repo ルートを必ず含む)。
/// 末尾 `/` 付き (ネストした未追跡 repo の `?? inner/` 形) も、
/// そのディレクトリ自身が最後の要素として得られる。
fn ancestor_dirs(path: &str) -> Vec<&str> {
    let mut v = vec![""];
    let mut idx = 0;
    while let Some(pos) = path[idx..].find('/') {
        idx += pos;
        if v.len() >= MAX_DIR_DEPTH {
            break;
        }
        v.push(&path[..idx]);
        idx += 1;
    }
    v
}

/// `status --porcelain=v1 -z` の出力をパースする。
///
/// NUL 区切りなのでパスの引用・エスケープが無く、リネームは
/// `XY <new>\0<old>\0` の 2 トークン組で届く。
/// `cap` 件を超えたら打ち切り、取り込めた分だけ返す (巨大 repo の劣化運転)。
fn parse_porcelain_z(output: &str, cap: usize) -> ParsedStatus {
    let mut out = ParsedStatus::default();
    let mut tokens = output.split('\0');
    while let Some(tok) = tokens.next() {
        if out.files.len() >= cap {
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
        // リネーム/コピーは次のトークンが旧パス。ここで消費する。
        let orig = if x == 'R' || x == 'C' || y == 'R' || y == 'C' {
            tokens.next().filter(|s| !s.is_empty()).map(str::to_string)
        } else {
            None
        };
        if let Some(status) = effective_status(x, y) {
            if let Some(o) = orig {
                if o != path {
                    out.renames.push((o, path.to_string()));
                }
            }
            out.files.insert(path.to_string(), status);
        }
    }
    out
}

/// 変更ファイル群から祖先ディレクトリの集計を導出する。
///
/// キーは相対ディレクトリ ("" = repo ルート)。値は (代表ステータス, 件数)。
/// VS Code 同様、配下が単一種ならその色、混在なら Modified の色調で塗る。
///
/// **深さは問わない**: `a/b/c/d/e/f/g.rs` の 1 変更で `a` … `a/b/c/d/e/f` と
/// ルート `""` の全てに色と件数が乗る (折りたたんだままでも「下で何か
/// 変わった」が見える)。スキャン 1 回につき 1 度だけ計算し、描画側は
/// このマップを O(1) で引くだけ (毎フレームのツリー走査をしない)。
///
/// リネームは旧パス側の親も色付けるが、**新パスと共有する祖先では
/// 件数を二重に数えない** (ルートの件数が変更ファイル数と食い違わないように)。
fn derive_dir_status(
    files: &HashMap<String, FileStatus>,
    renames: &[(String, String)],
) -> HashMap<String, (FileStatus, usize)> {
    let mut dirs: HashMap<String, (FileStatus, usize)> = HashMap::new();
    let mut bump = |dir: &str, st: FileStatus, count: bool| {
        let e = dirs.entry(dir.to_string()).or_insert((st, 0));
        if count {
            e.1 += 1;
        }
        if e.0 != st {
            e.0 = FileStatus::Modified;
        }
    };
    for (path, st) in files {
        for d in ancestor_dirs(path) {
            bump(d, *st, true);
        }
    }
    for (old, new) in renames {
        let shared: Vec<&str> = ancestor_dirs(new);
        for d in ancestor_dirs(old) {
            // 新パスと共有する祖先は既に新パス側で 1 件数えている。
            bump(d, FileStatus::Renamed, !shared.contains(&d));
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
    let parsed = parse_porcelain_z(&text, MAX_STATUS_ENTRIES);
    let dirs = derive_dir_status(&parsed.files, &parsed.renames);
    let deleted_by_dir = derive_deleted_by_dir(&parsed.files);
    let files = parsed.files;
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

// ═════════════════════════════════════════════════════════════════════
//  ブランチ切り替え (ツールバーのブランチボタン)
//
//  【方針】
//  - git は **1 度も UI スレッドで実行しない**。収集も切り替えもスレッド +
//    チャネル + TTL キャッシュ (このファイルの status スキャンと同じ形)。
//  - 収集はポップアップを開いている間だけ。閉じているフレームでは git を
//    1 本も起動しない (アイドル時のコストをゼロに保つ)。
//  - パースは git_panel.rs の既存パーサ (`parse_branch_list` /
//    `parse_worktree_porcelain` / `validate_branch_name`) をそのまま使う。
//    同じ出力の第 2 実装を持たない。
//  - **stash は絶対に使わない**。stash スタックは worktree 間で共有され、
//    別の worktree で作業している人の退避と混ざるため (CLAUDE.md)。
// ═════════════════════════════════════════════════════════════════════

/// 一覧に載せるブランチ数の上限 (ローカル / リモート追跡それぞれ)。
/// 超えた分は落とし、UI 側に「上限で切った」旨を出す。
pub const BRANCH_LIST_CAP: usize = 50;
/// 「変更があるので切り替えない」と伝えるときに名前を挙げるファイル数。
pub const DIRTY_NAMES_SHOWN: usize = 3;
/// 拒否メッセージ用に集める変更ファイル数の上限 (全件は数えるが名前は持たない)。
const DIRTY_SCAN_CAP: usize = 2_000;
/// ブランチ一覧スナップショットの TTL。
const BRANCH_NAV_TTL: Duration = Duration::from_secs(5);
/// `git switch` が入ったバージョン (2.23)。これ未満は `git checkout` を使う。
const SWITCH_SINCE: (u32, u32) = (2, 23);

/// 切り替え先。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SwitchTarget {
    /// ローカルブランチへ切り替える。
    Local(String),
    /// リモート追跡ブランチ (`origin/foo`) から追跡ローカルブランチを作る。
    Remote(String),
}

/// 切り替えを断る理由。**断ったときは何も変更しない** (stash もしない)。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SwitchBlock {
    /// すでにそのブランチに居る。
    AlreadyOn(String),
    /// 作業ツリーに未コミットの変更がある。
    Dirty { names: Vec<String>, total: usize },
    /// マージ / リベース等の途中。
    InProgress(String),
    /// 別の worktree がそのブランチを持っている (git は必ず拒否する)。
    OtherWorktree { branch: String, path: PathBuf },
    /// ブランチ名として受け付けられない。
    BadName(String),
}

impl SwitchBlock {
    /// ユーザーに出す説明文。
    pub fn message(&self) -> String {
        match self {
            SwitchBlock::AlreadyOn(b) => {
                trf("すでに {b} に居ます", &[("b", b.clone())])
            }
            SwitchBlock::Dirty { names, total } => {
                let head = names.join(", ");
                let rest = total.saturating_sub(names.len());
                if rest > 0 {
                    trf(
                        "未コミットの変更があるので切り替えません: {head} ほか {rest} 件",
                        &[("head", head), ("rest", rest.to_string())],
                    )
                } else {
                    trf(
                        "未コミットの変更があるので切り替えません: {head}",
                        &[("head", head)],
                    )
                }
            }
            SwitchBlock::InProgress(what) => trf(
                "{what}のため切り替えません (先に完了か中止をしてください)",
                &[("what", what.clone())],
            ),
            SwitchBlock::OtherWorktree { branch, path } => trf(
                "{b} は別の作業ツリーで開かれています: {p}",
                &[("b", branch.clone()), ("p", path.display().to_string())],
            ),
            SwitchBlock::BadName(n) => {
                trf("ブランチ名として使えません: {n}", &[("n", n.clone())])
            }
        }
    }

    /// 「変更をレビュー」への導線を出すべき拒否か。
    pub fn offers_review(&self) -> bool {
        matches!(self, SwitchBlock::Dirty { .. })
    }
}

/// リモート追跡ブランチ名 (`origin/feature/x`) → 作るローカル名 (`feature/x`)。
/// リモート名だけ・`origin/HEAD` は対象外 (None)。
pub fn local_branch_for_remote(remote_ref: &str) -> Option<String> {
    let (_remote, rest) = remote_ref.split_once('/')?;
    if rest.is_empty() || rest == "HEAD" {
        return None;
    }
    Some(rest.to_string())
}

/// `git --version` の出力から (major, minor) を取り出す。
pub fn parse_git_version(out: &str) -> Option<(u32, u32)> {
    // "git version 2.39.3 (Apple Git-145)" / "git version 2.45.1.windows.1"
    let tail = out.split_whitespace().find(|w| w.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut it = tail.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().unwrap_or("0");
    let minor = minor
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or(0);
    Some((major, minor))
}

/// この git が `git switch` を持っているか (2.23+)。判別できなければ false =
/// 昔からある `git checkout` を使う (「推測しない」)。
pub fn supports_switch(version_out: &str) -> bool {
    match parse_git_version(version_out) {
        Some(v) => v >= SWITCH_SINCE,
        None => false,
    }
}

/// 切り替えコマンドの引数列。`git -C <repo>` の後ろに続く部分だけを返す。
pub fn switch_argv(target: &SwitchTarget, supports_switch: bool) -> Vec<String> {
    match target {
        SwitchTarget::Local(n) => {
            if supports_switch {
                vec!["switch".into(), n.clone()]
            } else {
                vec!["checkout".into(), n.clone()]
            }
        }
        SwitchTarget::Remote(r) => {
            let local = local_branch_for_remote(r).unwrap_or_else(|| r.clone());
            if supports_switch {
                // 追跡ローカルブランチを git に作らせる (名前はリモート側から導出)。
                vec!["switch".into(), "--track".into(), r.clone()]
            } else {
                vec![
                    "checkout".into(),
                    "-b".into(),
                    local,
                    "--track".into(),
                    r.clone(),
                ]
            }
        }
    }
}

/// `git worktree list --porcelain` から「**自分以外の** worktree が持っている
/// ブランチ」の表を作る。ここに載っているブランチへは git が必ず切り替えを拒む。
pub fn worktree_holders(porcelain: &str, current_top: &Path) -> Vec<(String, PathBuf)> {
    let here = crate::pathx::canonical(current_top);
    crate::git_panel::parse_worktree_porcelain(porcelain)
        .into_iter()
        .filter(|w| !w.bare && !w.detached)
        .filter_map(|w| w.branch.clone().map(|b| (b, w.path)))
        .filter(|(_, p)| crate::pathx::canonical(p) != here)
        .collect()
}

/// マージ / リベース等の途中かどうか。`git_dir` は **その worktree の**
/// git ディレクトリ (`rev-parse --git-dir`。共有の common-dir ではない)。
pub fn in_progress_label(git_dir: &Path) -> Option<String> {
    // 目印ファイル → 表示名。git のドキュメントにある名前をそのまま使う。
    const TABLE: &[(&str, &str)] = &[
        ("rebase-merge", "リベース"),
        ("rebase-apply", "リベース"),
        ("MERGE_HEAD", "マージ"),
        ("CHERRY_PICK_HEAD", "チェリーピック"),
        ("REVERT_HEAD", "リバート"),
        ("BISECT_LOG", "二分探索"),
    ];
    TABLE
        .iter()
        .find(|(f, _)| git_dir.join(f).exists())
        .map(|(_, label)| tr(label))
}

/// 絞り込み (部分一致・ASCII の大文字小文字は無視)。空文字は素通し。
pub fn matches_filter(name: &str, filter: &str) -> bool {
    let f = filter.trim();
    if f.is_empty() {
        return true;
    }
    name.to_lowercase().contains(&f.to_lowercase())
}

/// ブランチ切り替えの判断に要る 1 回ぶんの収集結果。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct BranchSnapshot {
    /// 現在のブランチ (detached なら None)。
    pub head: Option<String>,
    /// detached HEAD のときの表示 ("HEAD detached at abc1234" 等)。
    pub detached: Option<String>,
    /// ローカルブランチ (最終コミットの新しい順)。
    pub local: Vec<crate::git_panel::BranchEntry>,
    /// リモート追跡ブランチ (`origin/…`。同じく新しい順)。
    pub remote: Vec<String>,
    /// 上限で切る前の件数。`local.len()` より大きければ切られている。
    pub local_total: usize,
    pub remote_total: usize,
    /// 未コミットの変更ファイル (追跡対象のみ。名前は先頭数件)。
    pub dirty: Vec<String>,
    pub dirty_total: usize,
    /// マージ / リベース等の途中ならその表示名。
    pub in_progress: Option<String>,
    /// `git switch` が使えるか (使えなければ `git checkout`)。
    pub supports_switch: bool,
    /// ブランチ名 → それを開いている **別の** worktree のパス。
    pub holders: Vec<(String, PathBuf)>,
}

impl BranchSnapshot {
    /// 切り替えて良いかを決める **純関数**。
    ///
    /// 通れば `git -C <repo>` に続く引数列、駄目なら理由を返す。
    /// 未追跡ファイル (`??`) は git の切り替えで失われないので数に入れない
    /// — 入れると「ビルド生成物があるだけで一生切り替えられない」になる。
    pub fn plan_switch(&self, target: &SwitchTarget) -> Result<Vec<String>, SwitchBlock> {
        let raw = match target {
            SwitchTarget::Local(n) => n.clone(),
            SwitchTarget::Remote(r) => {
                local_branch_for_remote(r).ok_or_else(|| SwitchBlock::BadName(r.clone()))?
            }
        };
        let name = crate::git_panel::validate_branch_name(&raw).map_err(SwitchBlock::BadName)?;
        if self.head.as_deref() == Some(name.as_str()) {
            return Err(SwitchBlock::AlreadyOn(name));
        }
        if let Some(what) = &self.in_progress {
            return Err(SwitchBlock::InProgress(what.clone()));
        }
        if let Some((_, path)) = self.holders.iter().find(|(b, _)| *b == name) {
            return Err(SwitchBlock::OtherWorktree {
                branch: name,
                path: path.clone(),
            });
        }
        if !self.dirty.is_empty() {
            return Err(SwitchBlock::Dirty {
                names: self.dirty.iter().take(DIRTY_NAMES_SHOWN).cloned().collect(),
                total: self.dirty_total,
            });
        }
        // 追跡ブランチを作ろうとしている先に同名のローカルがもう在るなら、
        // 作成ではなく素の切り替えにする (`--track` は既存名では失敗する)。
        let effective = match target {
            SwitchTarget::Remote(_) if self.local.iter().any(|b| b.name == name) => {
                SwitchTarget::Local(name)
            }
            other => other.clone(),
        };
        Ok(switch_argv(&effective, self.supports_switch))
    }
}

/// ツールバーのブランチボタンが持つ状態。
///
/// UI は「開いている間だけ」[`BranchNav::ensure_fresh`] を呼ぶ。閉じている
/// フレームでは git を 1 本も起動しない。
pub struct BranchNav {
    repo: PathBuf,
    snap: Option<Arc<BranchSnapshot>>,
    last: Option<Instant>,
    pending: Option<Receiver<Option<BranchSnapshot>>>,
    /// 走行中の切り替えジョブ (同時に 1 つだけ)。
    job: Option<Receiver<Result<String, String>>>,
    job_label: String,
    /// 絞り込み入力。
    pub filter: String,
    /// 直近の拒否理由 (ポップアップに出す)。
    pub block: Option<SwitchBlock>,
    /// UI が選んだ切り替え先。呼び出し側 (app.rs) が毎フレーム take する。
    request: Option<SwitchTarget>,
    /// 「変更をレビュー」が押された。
    review_requested: bool,
    /// ポップアップが開いているか (開閉のエッジ検出用)。
    was_open: bool,
    /// 絞り込み入力へフォーカスを移したい。
    focus_wanted: bool,
}

impl BranchNav {
    pub fn new(repo: PathBuf) -> Self {
        Self {
            repo,
            snap: None,
            last: None,
            pending: None,
            job: None,
            job_label: String::new(),
            filter: String::new(),
            block: None,
            request: None,
            review_requested: false,
            was_open: false,
            focus_wanted: false,
        }
    }

    /// ワークスペースが変わったら一覧を捨てる (旧 repo のブランチへ
    /// 切り替えを発行できてしまわないように、飛行中の収集も受信口ごと捨てる)。
    pub fn set_repo(&mut self, repo: PathBuf) {
        if self.repo == repo {
            return;
        }
        self.repo = repo;
        self.snap = None;
        self.last = None;
        self.pending = None;
        self.filter.clear();
        self.block = None;
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// 収集結果 (無ければ None)。Arc なので描画中に clone しても複製されない。
    pub fn snapshot(&self) -> Option<Arc<BranchSnapshot>> {
        self.snap.clone()
    }

    /// 収集中か。
    pub fn loading(&self) -> bool {
        self.pending.is_some()
    }

    /// 切り替えジョブが走行中か。
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    pub fn job_label(&self) -> &str {
        &self.job_label
    }

    /// ポップアップの開閉を伝える。開いた瞬間だけ絞り込みと拒否表示を素に戻し、
    /// 入力欄へフォーカスを要求する (開いている間ずっと奪わない)。
    pub fn set_open(&mut self, open: bool) {
        if open == self.was_open {
            return;
        }
        self.was_open = open;
        if open {
            self.filter.clear();
            self.block = None;
            self.focus_wanted = true;
        }
    }

    /// 「入力欄へフォーカスを移したい」を 1 回だけ受け取る。
    pub fn take_focus_request(&mut self) -> bool {
        std::mem::take(&mut self.focus_wanted)
    }

    /// UI からの選択。判断は [`BranchSnapshot::plan_switch`] に任せ、
    /// ここでは「実行を要求した」か「拒否理由を出した」かだけを記録する。
    pub fn select(&mut self, target: SwitchTarget) {
        let Some(snap) = self.snap.clone() else {
            return;
        };
        match snap.plan_switch(&target) {
            Ok(_) => {
                self.block = None;
                self.request = Some(target);
            }
            Err(b) => self.block = Some(b),
        }
    }

    /// 拒否メッセージの「変更をレビュー」。
    pub fn request_review(&mut self) {
        self.review_requested = true;
    }

    pub fn take_review_request(&mut self) -> bool {
        std::mem::take(&mut self.review_requested)
    }

    pub fn take_request(&mut self) -> Option<SwitchTarget> {
        self.request.take()
    }

    /// TTL 切れなら収集をバックグラウンドで仕込む。**開いている間だけ呼ぶこと**。
    pub fn ensure_fresh(&mut self, ctx: &egui::Context) {
        self.poll_scan();
        if self.pending.is_some() || self.job.is_some() {
            return;
        }
        if let Some(t) = self.last {
            if t.elapsed() < BRANCH_NAV_TTL {
                return;
            }
        }
        self.spawn_scan(ctx);
    }

    /// 収集を今すぐ取り直させる (切り替え完了後など)。
    pub fn invalidate(&mut self) {
        self.last = None;
    }

    fn poll_scan(&mut self) {
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(snap) => {
                    self.snap = snap.map(Arc::new);
                    self.pending = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    fn spawn_scan(&mut self, ctx: &egui::Context) {
        // 失敗しても時刻は進める (毎フレーム spawn を試みない)。
        self.last = Some(Instant::now());
        let (tx, rx) = mpsc::channel();
        let repo = self.repo.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("zv-git-branches".into())
            .spawn(move || {
                let _ = tx.send(scan_branches(&repo));
                ctx.request_repaint();
            });
        if spawned.is_ok() {
            self.pending = Some(rx);
        }
    }

    /// 実際の切り替えを走らせる。**UI スレッドでは git を動かさない**。
    pub fn start_switch(&mut self, argv: Vec<String>, label: String, ctx: &egui::Context) {
        if self.job.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let repo = self.repo.clone();
        let ctx2 = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("zv-git-switch".into())
            .spawn(move || {
                let _ = tx.send(run_git_at(&repo, &argv));
                ctx2.request_repaint();
            });
        if spawned.is_ok() {
            self.job = Some(rx);
            self.job_label = label;
        }
    }

    /// 完了した切り替えジョブを回収する。`Some((メッセージ, 成功か))`。
    pub fn poll_job(&mut self) -> Option<(String, bool)> {
        let rx = self.job.as_ref()?;
        let res = match rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.job = None;
                return None;
            }
            Err(mpsc::TryRecvError::Empty) => return None,
        };
        self.job = None;
        let label = std::mem::take(&mut self.job_label);
        self.last = None; // 一覧は必ず取り直す
        Some(match res {
            Ok(_) => (
                trf("🌿 {label} に切り替えました", &[("label", label)]),
                true,
            ),
            // git の拒否理由は加工せずそのまま見せる。
            Err(e) => (
                trf(
                    "切り替えに失敗しました ({label}): {e}",
                    &[("label", label), ("e", e)],
                ),
                false,
            ),
        })
    }
}

/// `git -C <dir> <args>` を同期実行して stdout を返す。失敗は stderr の文言。
/// 呼ぶ側がスレッドを用意すること。
fn run_git_at(dir: &Path, args: &[String]) -> Result<String, String> {
    let out = crate::procx::hidden_command("git")
        // color.ui=always / core.quotepath=true な設定でも素の UTF-8 を得る
        // (ブランチ名の日本語がエスケープされるとパースも表示も壊れる)。
        .args(["-c", "color.ui=false", "-c", "core.quotepath=false", "-C"])
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = crate::textenc::decode_output(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            trf("git {args} が失敗しました", &[("args", args.join(" "))])
        } else {
            err
        });
    }
    Ok(crate::textenc::decode_output(&out.stdout))
}

fn git_text(dir: &Path, args: &[&str]) -> Option<String> {
    let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    run_git_at(dir, &owned).ok()
}

/// worktree をまたいでも同じ値になる **リポジトリの識別キー**。
///
/// `--git-common-dir` は本体 worktree でもリンク worktree でも同じ
/// (共有の `.git`) を指すので、「同じリポジトリか」の判定に使える。
/// `--path-format=absolute` は git 2.31+ なので、無い場合は相対パスを
/// `dir` 基準で絶対化する。どちらも駄目なら `--git-dir` へ落とす。
pub fn repo_key(dir: &Path) -> Option<PathBuf> {
    let abs = |raw: String| -> Option<PathBuf> {
        let t = raw.trim();
        if t.is_empty() {
            return None;
        }
        let p = PathBuf::from(t);
        let p = if p.is_absolute() { p } else { dir.join(p) };
        Some(crate::pathx::canonical(&p))
    };
    for args in [
        &["rev-parse", "--path-format=absolute", "--git-common-dir"][..],
        &["rev-parse", "--git-common-dir"][..],
        &["rev-parse", "--path-format=absolute", "--git-dir"][..],
        &["rev-parse", "--git-dir"][..],
    ] {
        if let Some(v) = git_text(dir, args).and_then(abs) {
            return Some(v);
        }
    }
    None
}

/// 「同じリポジトリに属するフォルダ」の集合。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct RepoFamily {
    /// このリポジトリの worktree (bare は除く。本体とリンク先の両方)。
    pub worktrees: Vec<PathBuf>,
    /// 候補フォルダのうち、識別キーが一致したもの (順は入力のまま)。
    /// worktree のサブディレクトリを開いている場合もここで拾える。
    pub related: Vec<PathBuf>,
}

/// `repo` と同じリポジトリに属するフォルダを集める (**バックグラウンド専用**)。
///
/// 判定は [`repo_key`] (= `--git-common-dir`) の一致。worktree ごとに
/// `.git` の実体は違っても common-dir は共有なので、ブランチごとに
/// フォルダを分けていても「同じリポジトリ」と分かる。
pub fn scan_repo_family(repo: &Path, candidates: &[PathBuf]) -> RepoFamily {
    let Some(key) = repo_key(repo) else {
        return RepoFamily::default();
    };
    let worktrees = worktree_paths(repo);
    let related = candidates
        .iter()
        .map(|c| crate::pathx::canonical(c))
        .filter(|c| !worktrees.contains(c))
        .filter(|c| repo_key(c).as_ref() == Some(&key))
        .collect();
    RepoFamily { worktrees, related }
}

/// リポジトリに属する worktree のパス一覧 (bare は除く)。
pub fn worktree_paths(dir: &Path) -> Vec<PathBuf> {
    let Some(out) = git_text(dir, &["worktree", "list", "--porcelain"]) else {
        return Vec::new();
    };
    crate::git_panel::parse_worktree_porcelain(&out)
        .into_iter()
        .filter(|w| !w.bare)
        .map(|w| crate::pathx::canonical(&w.path))
        .collect()
}

/// ブランチ切り替えに要る情報を 1 回で集める (バックグラウンドスレッド専用)。
fn scan_branches(repo: &Path) -> Option<BranchSnapshot> {
    let toplevel = git_text(repo, &["rev-parse", "--show-toplevel"])
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty())?;

    // 最終コミットの新しい順。`for-each-ref` ではなく `branch --all` を使うのは、
    // 同じ ref 走査に加えて `*` (この worktree の HEAD) と `+` (別 worktree で
    // 使用中) のマーカーが同時に得られ、既存パーサをそのまま使えるため。
    // `--sort` は git 2.7+ なので、受け付けなければ素の一覧へ落とす。
    let raw = git_text(repo, &["branch", "--all", "--sort=-committerdate"])
        .or_else(|| git_text(repo, &["branch", "--all"]))
        .unwrap_or_default();
    let list = crate::git_panel::parse_branch_list(&raw);

    let (head, detached) = match list.head.clone() {
        Some(crate::git_panel::HeadState::OnBranch(b)) => (Some(b), None),
        Some(crate::git_panel::HeadState::Detached(d)) => (None, Some(d)),
        _ => (None, None),
    };

    let local_total = list.local.len();
    let remote_total = list.remote.len();
    let mut local = list.local;
    local.truncate(BRANCH_LIST_CAP);
    let mut remote = list.remote;
    remote.truncate(BRANCH_LIST_CAP);

    // 未コミットの変更 (追跡対象のみ)。名前は表示ぶんだけ持つ。
    let status = git_text(repo, &["status", "--porcelain=v1", "-z"]).unwrap_or_default();
    let parsed = parse_porcelain_z(&status, DIRTY_SCAN_CAP);
    let mut dirty: Vec<String> = parsed
        .files
        .into_iter()
        .filter(|(_, s)| *s != FileStatus::Untracked)
        .map(|(p, _)| p)
        .collect();
    dirty.sort();
    let dirty_total = dirty.len();
    dirty.truncate(DIRTY_NAMES_SHOWN);

    let holders = git_text(repo, &["worktree", "list", "--porcelain"])
        .map(|s| worktree_holders(&s, &toplevel))
        .unwrap_or_default();

    // この worktree 固有の git ディレクトリ (MERGE_HEAD 等はここに置かれる)。
    let git_dir = git_text(repo, &["rev-parse", "--path-format=absolute", "--git-dir"])
        .or_else(|| git_text(repo, &["rev-parse", "--git-dir"]))
        .map(|s| {
            let p = PathBuf::from(s.trim());
            if p.is_absolute() {
                p
            } else {
                repo.join(p)
            }
        });
    let in_progress = git_dir.as_deref().and_then(in_progress_label);

    let supports = git_text(repo, &["--version"])
        .map(|v| supports_switch(&v))
        .unwrap_or(false);

    Some(BranchSnapshot {
        head,
        detached,
        local,
        remote,
        local_total,
        remote_total,
        dirty,
        dirty_total,
        in_progress,
        supports_switch: supports,
        holders,
    })
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
        let dir = crate::test_util::unique_temp_dir("zaivern-git-test", tag);
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

    /// マルチルート: 各ルートが独立に集計され、どのルートにも属さない
    /// パスは無視される (別ワークスペースの色が混ざらない)。
    #[test]
    fn multi_root_workspaces_stay_isolated() {
        let (Some(r1), Some(r2)) = (temp_repo("multi-a"), temp_repo("multi-b")) else {
            return;
        };
        // 未追跡ディレクトリは status が `?? deep/` と畳んで報告するため、
        // 「深い階層の 1 ファイルだけ変更」を作るには一度コミットしておく。
        let commit = |dir: &PathBuf, name: &str, body: &str| {
            std::fs::create_dir_all(dir.join("deep/one/two")).expect("mkdir");
            std::fs::write(dir.join("deep/one/two").join(name), "base\n").expect("seed");
            let git = |args: &[&str]| {
                Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(["-c", "user.name=zv", "-c", "user.email=zv@example.com"])
                    .args(args)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            };
            git(&["add", "-A"]);
            git(&["commit", "--quiet", "-m", "seed"]);
            std::fs::write(dir.join("deep/one/two").join(name), body).expect("modify");
        };
        commit(&r1, "a.rs", "a changed\n");
        commit(&r2, "b.rs", "b changed\n");

        let c1 = crate::pathx::canonical(&r1);
        let c2 = crate::pathx::canonical(&r2);
        let mut set = GitSet::new(vec![c1.clone(), c2.clone()]);
        assert_eq!(set.repo_count(), 2, "別 repo は別々に持つ");

        // 同期的にスキャンを取り込む (バックグラウンドの完了を待つ)
        for _ in 0..200 {
            set.refresh_if_stale();
            if set.dirty_count() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        assert_eq!(
            set.file_status(&c1.join("deep/one/two/a.rs")),
            Some(FileStatus::Modified)
        );
        assert_eq!(
            set.file_status(&c2.join("deep/one/two/b.rs")),
            Some(FileStatus::Modified)
        );
        // ルート 1 の集計にルート 2 のファイルは入らない
        assert_eq!(set.file_status(&c1.join("deep/one/two/b.rs")), None);
        assert_eq!(set.dir_status(&c1.join("deep/one")).map(|d| d.1), Some(1));
        assert_eq!(set.dir_status(&c2.join("deep/one")).map(|d| d.1), Some(1));
        // どのルートにも属さないパスは完全に無視する
        let outside = crate::pathx::canonical(&std::env::temp_dir()).join("zv-not-a-root/x.rs");
        assert_eq!(set.file_status(&outside), None);
        assert_eq!(set.dir_status(&outside), None);
        assert!(set.deleted_names_in(&outside).is_empty());

        std::fs::remove_dir_all(&r1).ok();
        std::fs::remove_dir_all(&r2).ok();
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
        let parsed = parse_porcelain_z(out, MAX_STATUS_ENTRIES);
        assert_eq!(
            parsed.renames,
            vec![("old.rs".to_string(), "renamed.rs".to_string())],
            "リネームは (旧, 新) の組で取り出す"
        );
        let map = parsed.files;
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
        let map = parse_porcelain_z(out, MAX_STATUS_ENTRIES).files;
        assert_eq!(map.get("my notes.txt"), Some(&FileStatus::Untracked));
        assert_eq!(map.get("dir with space/a.rs"), Some(&FileStatus::Modified));

        // cap 超過は打ち切り、取り込めた分だけ返す (劣化運転)
        let capped = parse_porcelain_z(out, 2);
        assert_eq!(capped.files.len(), 2);
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
        let dirs = derive_dir_status(&files, &[]);
        // 混在 (M + A) は Modified の色調に寄せる (VS Code 挙動)
        assert_eq!(dirs.get("a/b"), Some(&(FileStatus::Modified, 2)));
        assert_eq!(dirs.get("a"), Some(&(FileStatus::Modified, 3)));
        assert_eq!(dirs.get(""), Some(&(FileStatus::Modified, 4)));
        assert!(!dirs.contains_key("a/b/c.rs"), "ファイル自身は含まない");

        // 単一種ならその色のまま
        let mut only_added = HashMap::new();
        only_added.insert("pkg/one.rs".to_string(), FileStatus::Added);
        only_added.insert("pkg/two.rs".to_string(), FileStatus::Added);
        let dirs = derive_dir_status(&only_added, &[]);
        assert_eq!(dirs.get("pkg"), Some(&(FileStatus::Added, 2)));
        assert_eq!(dirs.get(""), Some(&(FileStatus::Added, 2)));
    }

    // ── 深い階層 / 折りたたみ / リネーム / 削除 / 上限 ────────────────

    /// 6 階層より深い 1 変更でも、**ルートまでの全祖先**に色と件数が乗る。
    /// (折りたたんだフォルダ行も dir_status を引くだけなので、
    ///  展開しなくても「下で何か変わった」がバッジで見える)
    #[test]
    fn deep_hierarchy_tints_every_ancestor_up_to_root() {
        let deep = "a/b/c/d/e/f/g/deep.rs";
        let mut files = HashMap::new();
        files.insert(deep.to_string(), FileStatus::Modified);
        let dirs = derive_dir_status(&files, &[]);

        for anc in ["", "a", "a/b", "a/b/c", "a/b/c/d", "a/b/c/d/e", "a/b/c/d/e/f", "a/b/c/d/e/f/g"] {
            assert_eq!(
                dirs.get(anc),
                Some(&(FileStatus::Modified, 1)),
                "祖先 {anc:?} も色付く"
            );
        }
        assert_eq!(dirs.len(), 8, "ファイル自身はディレクトリに含めない");
        assert!(!dirs.contains_key(deep));
    }

    /// 折りたたんだままのフォルダ行が読む API (`Git::dir_status`) が、
    /// 深い階層でも代表ステータスと件数を返すこと。
    #[test]
    fn collapsed_folder_sees_status_and_count_without_expanding() {
        let mut g = Git::new(PathBuf::from("/nowhere"));
        let mut files = HashMap::new();
        files.insert("src/deep/one/two/three/x.rs".to_string(), FileStatus::Added);
        files.insert("src/deep/one/two/three/y.rs".to_string(), FileStatus::Added);
        files.insert("src/other.rs".to_string(), FileStatus::Modified);
        g.dir_cache = derive_dir_status(&files, &[]);
        g.status_cache = files;

        // 一番浅い折りたたみ行 (src) からでも配下 3 件が見える
        assert_eq!(g.dir_status("src"), Some((FileStatus::Modified, 3)));
        // 中間層は 2 件・単一種なので Added のまま
        assert_eq!(g.dir_status("src/deep"), Some((FileStatus::Added, 2)));
        assert_eq!(g.dir_status("src/deep/one/two/three"), Some((FileStatus::Added, 2)));
        // 末尾スラッシュ付きでも同じキーに解決する
        assert_eq!(g.dir_status("src/deep/"), Some((FileStatus::Added, 2)));
        assert_eq!(g.dir_status("vendor"), None);
    }

    /// リネームは旧側の親も色付く。共有する祖先では二重に数えない。
    #[test]
    fn rename_tints_both_parent_chains_without_double_counting() {
        let parsed = parse_porcelain_z("R  new/deep/dst.rs\0old/deep/src.rs\0", MAX_STATUS_ENTRIES);
        let dirs = derive_dir_status(&parsed.files, &parsed.renames);

        for anc in ["new", "new/deep"] {
            assert_eq!(dirs.get(anc), Some(&(FileStatus::Renamed, 1)), "新側 {anc}");
        }
        for anc in ["old", "old/deep"] {
            assert_eq!(dirs.get(anc), Some(&(FileStatus::Renamed, 1)), "旧側 {anc}");
        }
        // 共有する祖先 (ルート) はリネーム 1 件ぶんだけ
        assert_eq!(dirs.get(""), Some(&(FileStatus::Renamed, 1)));
    }

    /// 削除はワークツリーから消えていても親を色付ける。
    #[test]
    fn delete_tints_parent_chain_even_though_file_is_gone() {
        let parsed = parse_porcelain_z(" D a/b/c/d/e/f/gone.rs\0", MAX_STATUS_ENTRIES);
        let dirs = derive_dir_status(&parsed.files, &parsed.renames);
        for anc in ["", "a", "a/b", "a/b/c", "a/b/c/d", "a/b/c/d/e", "a/b/c/d/e/f"] {
            assert_eq!(dirs.get(anc), Some(&(FileStatus::Deleted, 1)), "{anc}");
        }
        // 幽霊行 (消えたファイル名) も親ディレクトリから引ける
        let ghosts = derive_deleted_by_dir(&parsed.files);
        assert_eq!(ghosts.get("a/b/c/d/e/f").map(Vec::as_slice), Some(&["gone.rs".to_string()][..]));
    }

    /// サブモジュール / ネストした repo の扱い。
    /// - 外側の status には「サブモジュールのディレクトリ 1 エントリ」しか出ない
    ///   → 内側の個別ファイルが外側に誤って割り当てられない
    /// - それでもフォルダ行には色が出る (file エントリへのフォールバック)
    #[test]
    fn nested_repo_and_submodule_do_not_leak_into_outer_repo() {
        // ` M sub` = サブモジュールの HEAD 移動、`?? nested/` = ネストした未追跡 repo
        let parsed = parse_porcelain_z(" M sub\0?? nested/\0 M src/app.rs\0", MAX_STATUS_ENTRIES);
        let mut g = Git::new(PathBuf::from("/nowhere"));
        g.dir_cache = derive_dir_status(&parsed.files, &parsed.renames);
        g.status_cache = parsed.files;

        // サブモジュール行は色が出る (件数 1)
        assert_eq!(g.dir_status("sub"), Some((FileStatus::Modified, 1)));
        // ネストした未追跡 repo も同じ
        assert_eq!(g.dir_status("nested"), Some((FileStatus::Untracked, 1)));
        // 中身は外側の status に無い → 誤った色付けをしない
        assert_eq!(g.file_status("sub/lib/inner.rs"), None);
        assert_eq!(g.dir_status("sub/lib"), None);
        assert_eq!(g.file_status("nested/x.rs"), None);
        assert_eq!(g.dir_status("nested/x"), None);
    }

    /// 1 万件クラスの変更でも上限で頭打ちにし、祖先集計が破綻しない。
    /// (祖先マップはスキャン 1 回につき 1 度だけ組み立てる = 描画は O(1) 参照)
    #[test]
    fn ten_thousand_changes_stay_within_caps() {
        let mut raw = String::new();
        for i in 0..12_000 {
            raw.push_str(&format!(" M pkg{}/mod{}/file{}.rs\0", i % 40, i % 400, i));
        }
        let parsed = parse_porcelain_z(&raw, MAX_STATUS_ENTRIES);
        assert_eq!(parsed.files.len(), MAX_STATUS_ENTRIES, "上限で打ち切る");
        let dirs = derive_dir_status(&parsed.files, &parsed.renames);
        // ルートは取り込めた件数ぶん、ディレクトリ数はパス空間ぶんで抑えられる
        assert_eq!(dirs.get("").map(|d| d.1), Some(MAX_STATUS_ENTRIES));
        assert!(dirs.len() <= 1 + 40 + 400, "ディレクトリ集計が爆発しない: {}", dirs.len());
    }

    /// 病的に深いパスでも祖先の展開を打ち切る (メモリ暴走止め)。
    #[test]
    fn pathological_depth_is_capped() {
        let deep: String = (0..500).map(|i| format!("d{i}/")).collect::<String>() + "x.rs";
        let ancs = ancestor_dirs(&deep);
        assert_eq!(ancs.len(), MAX_DIR_DEPTH);
        assert_eq!(ancs[0], "");
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

    // ══════════════ ブランチ切り替え ══════════════

    /// `git --version` の表記ゆれ (Apple / Windows / 開発版) を吸収する。
    #[test]
    fn git_version_parse_table() {
        let table: &[(&str, Option<(u32, u32)>, bool)] = &[
            ("git version 2.39.3 (Apple Git-145)\n", Some((2, 39)), true),
            ("git version 2.45.1.windows.1\n", Some((2, 45)), true),
            ("git version 2.23.0", Some((2, 23)), true),
            ("git version 2.22.0", Some((2, 22)), false),
            ("git version 1.9.5", Some((1, 9)), false),
            ("git version 3", Some((3, 0)), true),
            ("", None, false),
            ("なにか壊れた出力", None, false),
        ];
        for (raw, want, sw) in table {
            assert_eq!(parse_git_version(raw), *want, "版数: {raw:?}");
            assert_eq!(supports_switch(raw), *sw, "switch 可否: {raw:?}");
        }
    }

    /// リモート追跡名 → 作られるローカル名。
    #[test]
    fn local_branch_for_remote_table() {
        let table: &[(&str, Option<&str>)] = &[
            ("origin/main", Some("main")),
            ("origin/feature/login-v2", Some("feature/login-v2")),
            ("upstream/日本語ブランチ", Some("日本語ブランチ")),
            ("origin/HEAD", None),
            ("origin/", None),
            ("origin", None),
        ];
        for (raw, want) in table {
            assert_eq!(
                local_branch_for_remote(raw).as_deref(),
                *want,
                "入力: {raw}"
            );
        }
    }

    /// 切り替えコマンドの引数表 (**実行はしない**)。
    /// git 2.23+ は `switch`、それ未満は `checkout` へ落ちる。
    #[test]
    fn switch_argv_table() {
        let local = SwitchTarget::Local("feature/login-v2".into());
        assert_eq!(
            switch_argv(&local, true),
            vec!["switch".to_string(), "feature/login-v2".to_string()]
        );
        assert_eq!(
            switch_argv(&local, false),
            vec!["checkout".to_string(), "feature/login-v2".to_string()]
        );

        let remote = SwitchTarget::Remote("origin/feature/login-v2".into());
        assert_eq!(
            switch_argv(&remote, true),
            vec![
                "switch".to_string(),
                "--track".to_string(),
                "origin/feature/login-v2".to_string()
            ],
            "新しい git は --track だけでローカル名を導出する"
        );
        assert_eq!(
            switch_argv(&remote, false),
            vec![
                "checkout".to_string(),
                "-b".to_string(),
                "feature/login-v2".to_string(),
                "--track".to_string(),
                "origin/feature/login-v2".to_string()
            ],
            "古い git は -b <local> --track <remote>"
        );

        // 日本語のリモート追跡ブランチでもローカル名の導出が壊れない
        assert_eq!(
            switch_argv(&SwitchTarget::Remote("origin/機能/検索".into()), false),
            vec![
                "checkout".to_string(),
                "-b".to_string(),
                "機能/検索".to_string(),
                "--track".to_string(),
                "origin/機能/検索".to_string()
            ]
        );
    }

    /// `git branch --all` のパース: detached / `/` 入り / 非 ASCII / 別 worktree。
    #[test]
    fn branch_list_parsing_covers_detached_slash_and_unicode() {
        use crate::git_panel::HeadState;
        let raw = "\
* (HEAD detached at abc1234)
  feature/login-v2
+ night/2026-07-26
  日本語ブランチ
  main
  remotes/origin/HEAD -> origin/main
  remotes/origin/feature/login-v2
  remotes/origin/日本語ブランチ
";
        let list = crate::git_panel::parse_branch_list(raw);
        assert_eq!(
            list.head,
            Some(HeadState::Detached("HEAD detached at abc1234".into())),
            "detached HEAD は分離状態として拾う"
        );
        let names: Vec<&str> = list.local.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["feature/login-v2", "night/2026-07-26", "日本語ブランチ", "main"],
            "`/` 入りも非 ASCII も出力順のまま残る"
        );
        assert!(
            list.local.iter().find(|b| b.name == "night/2026-07-26").unwrap().other_worktree,
            "`+` は別 worktree で使用中",
        );
        assert!(!list.local.iter().any(|b| b.current), "detached なので current は無い");
        assert_eq!(
            list.remote,
            vec!["origin/feature/login-v2", "origin/日本語ブランチ"],
            "origin/HEAD -> … のシンボリック行は出さない"
        );
    }

    /// `git worktree list --porcelain` から「別の作業ツリーが持っているブランチ」を割る。
    #[test]
    fn worktree_holders_detects_branch_checked_out_elsewhere() {
        let here = std::env::temp_dir().join("zv-wt-main");
        let other = std::env::temp_dir().join("zv-wt-night");
        let porcelain = format!(
            "worktree {main}\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/main\n\
             \nworktree {night}\nHEAD 2222222222222222222222222222222222222222\nbranch refs/heads/night/2026-07-26\n\
             \nworktree {det}\nHEAD 3333333333333333333333333333333333333333\ndetached\n\
             \nworktree {bare}\nbare\n\n",
            main = here.display(),
            night = other.display(),
            det = std::env::temp_dir().join("zv-wt-detached").display(),
            bare = std::env::temp_dir().join("zv-wt-bare").display(),
        );

        let holders = worktree_holders(&porcelain, &here);
        assert_eq!(
            holders,
            vec![("night/2026-07-26".to_string(), other.clone())],
            "自分自身・detached・bare は除き、他 worktree のブランチだけが残る"
        );

        let snap = BranchSnapshot {
            head: Some("main".into()),
            holders,
            supports_switch: true,
            ..Default::default()
        };
        assert_eq!(
            snap.plan_switch(&SwitchTarget::Local("night/2026-07-26".into())),
            Err(SwitchBlock::OtherWorktree {
                branch: "night/2026-07-26".into(),
                path: other,
            }),
            "git が拒否する前にこちらで止め、どの作業ツリーが持っているか言う"
        );
    }

    /// 作業ツリーが汚れているときは切り替えない (stash も**しない**)。
    /// 未追跡ファイルだけなら切り替えて構わない (git は失わない)。
    #[test]
    fn dirty_tree_refuses_and_untracked_only_does_not() {
        let dirty = BranchSnapshot {
            head: Some("main".into()),
            dirty: vec!["src/app.rs".into(), "src/git.rs".into(), "README.md".into()],
            dirty_total: 7,
            supports_switch: true,
            ..Default::default()
        };
        let err = dirty
            .plan_switch(&SwitchTarget::Local("feature/x".into()))
            .expect_err("汚れていたら断る");
        match &err {
            SwitchBlock::Dirty { names, total } => {
                assert_eq!(names.len(), DIRTY_NAMES_SHOWN);
                assert_eq!(*total, 7);
            }
            other => panic!("Dirty を期待: {other:?}"),
        }
        assert!(err.offers_review(), "「変更をレビュー」への導線を出す");
        let msg = err.message();
        assert!(msg.contains("src/app.rs"), "変更ファイル名を挙げる: {msg}");
        assert!(!msg.contains("stash"), "stash は提案しない: {msg}");

        // 未追跡だけ = dirty には入らない → 素通し
        let clean = BranchSnapshot {
            head: Some("main".into()),
            supports_switch: true,
            ..Default::default()
        };
        assert_eq!(
            clean.plan_switch(&SwitchTarget::Local("feature/x".into())),
            Ok(vec!["switch".to_string(), "feature/x".to_string()])
        );
    }

    /// マージ / リベース途中は断る。
    #[test]
    fn mid_merge_refuses_switch() {
        let snap = BranchSnapshot {
            head: Some("main".into()),
            in_progress: Some("マージ".into()),
            supports_switch: true,
            ..Default::default()
        };
        let err = snap
            .plan_switch(&SwitchTarget::Local("feature/x".into()))
            .expect_err("マージ中は断る");
        assert_eq!(err, SwitchBlock::InProgress("マージ".into()));
        assert!(!err.offers_review());
    }

    /// 途中状態の目印ファイル → 表示名。
    #[test]
    fn in_progress_label_reads_git_dir_markers() {
        let dir = crate::test_util::unique_temp_dir("zaivern-git-test", "inprogress");
        assert_eq!(in_progress_label(&dir), None, "何も無ければ None");

        std::fs::write(dir.join("MERGE_HEAD"), "abc\n").expect("write MERGE_HEAD");
        assert!(in_progress_label(&dir).is_some(), "マージ中を検出");
        std::fs::remove_file(dir.join("MERGE_HEAD")).ok();

        std::fs::create_dir_all(dir.join("rebase-merge")).expect("mkdir rebase-merge");
        assert!(in_progress_label(&dir).is_some(), "リベース中を検出");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// すでに居るブランチ / 不正な名前は実行しない。
    #[test]
    fn already_on_and_bad_names_are_refused() {
        let snap = BranchSnapshot {
            head: Some("main".into()),
            supports_switch: true,
            ..Default::default()
        };
        assert_eq!(
            snap.plan_switch(&SwitchTarget::Local("main".into())),
            Err(SwitchBlock::AlreadyOn("main".into()))
        );
        assert!(matches!(
            snap.plan_switch(&SwitchTarget::Local("--force".into())),
            Err(SwitchBlock::BadName(_))
        ));
        assert!(matches!(
            snap.plan_switch(&SwitchTarget::Remote("origin".into())),
            Err(SwitchBlock::BadName(_))
        ));
    }

    /// 同名のローカルがもう在るリモート追跡ブランチは、作成ではなく素の切り替え。
    #[test]
    fn remote_target_falls_back_to_plain_switch_when_local_exists() {
        let snap = BranchSnapshot {
            head: Some("main".into()),
            local: vec![crate::git_panel::BranchEntry {
                name: "feature/x".into(),
                current: false,
                other_worktree: false,
            }],
            supports_switch: true,
            ..Default::default()
        };
        assert_eq!(
            snap.plan_switch(&SwitchTarget::Remote("origin/feature/x".into())),
            Ok(vec!["switch".to_string(), "feature/x".to_string()]),
            "--track は既存名では失敗するので素の switch にする"
        );
    }

    #[test]
    fn filter_is_case_insensitive_substring() {
        assert!(matches_filter("feature/Login-V2", "login"));
        assert!(matches_filter("日本語ブランチ", "ブランチ"));
        assert!(matches_filter("main", "  "), "空欄は素通し");
        assert!(!matches_filter("main", "night"));
    }

    /// 実リポジトリでの収集。作業ツリーが汚れていることも拾う。
    #[test]
    fn scan_branches_real_fixture() {
        let Some(repo) = temp_repo("scanbranch") else {
            return;
        };
        commit_something(&repo, "a.txt", "one\n");
        let snap = scan_branches(&repo).expect("収集できる");
        assert!(snap.head.is_some(), "ブランチ名が取れる: {snap:?}");
        assert!(snap.dirty.is_empty(), "コミット直後は綺麗");
        assert!(snap.in_progress.is_none());
        assert!(snap.holders.is_empty(), "worktree は 1 つだけ");

        std::fs::write(repo.join("a.txt"), "two\n").expect("dirty it");
        let snap = scan_branches(&repo).expect("収集できる");
        assert_eq!(snap.dirty, vec!["a.txt".to_string()]);
        assert_eq!(snap.dirty_total, 1);
        assert!(
            snap.plan_switch(&SwitchTarget::Local("whatever".into())).is_err(),
            "汚れていれば切り替えない"
        );

        std::fs::remove_dir_all(&repo).ok();
    }

    /// リポジトリ識別キー: 同じ repo の 2 つの worktree は同じ値、別 repo は別の値。
    #[test]
    fn repo_key_is_shared_across_worktrees() {
        let Some(repo) = temp_repo("repokey-a") else {
            return;
        };
        commit_something(&repo, "a.txt", "one\n");
        let Some(other) = temp_repo("repokey-b") else {
            std::fs::remove_dir_all(&repo).ok();
            return;
        };
        commit_something(&other, "b.txt", "one\n");

        let wt = repo.join("wt-second");
        let added = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "-b", "second"])
            .arg(&wt)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        let key_main = repo_key(&repo).expect("本体の識別キー");
        assert_eq!(
            repo_key(&repo.join("sub-not-exist").parent().unwrap()),
            Some(key_main.clone()),
            "同じディレクトリなら当然同じ"
        );
        assert_ne!(
            repo_key(&other),
            Some(key_main.clone()),
            "別リポジトリは別のキー"
        );

        if added {
            assert_eq!(
                repo_key(&wt),
                Some(key_main.clone()),
                "リンク worktree でも同じリポジトリとして同じキーになる"
            );
            let paths = worktree_paths(&wt);
            assert!(
                paths.len() >= 2,
                "worktree 一覧は本体とリンク先の両方を含む: {paths:?}"
            );
            assert!(paths.contains(&crate::pathx::canonical(&wt)));

            // 候補フォルダの仕分け: 同じ repo のサブディレクトリは拾い、
            // 別 repo は落とす (セッション一覧がよそのリポジトリへ漏れない)。
            let sub = repo.join("sub");
            std::fs::create_dir_all(&sub).expect("mkdir sub");
            let fam = scan_repo_family(
                &wt,
                &[sub.clone(), other.clone(), std::env::temp_dir().join("zv-nope")],
            );
            assert!(fam.worktrees.contains(&crate::pathx::canonical(&wt)));
            assert_eq!(
                fam.related,
                vec![crate::pathx::canonical(&sub)],
                "同じ repo のサブディレクトリだけが関連フォルダになる"
            );
        }

        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    /// テスト用: 1 コミット作る (worktree 追加には最低 1 コミット要る)。
    fn commit_something(repo: &Path, name: &str, body: &str) {
        std::fs::write(repo.join(name), body).expect("write file");
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .expect("git run");
        };
        run(&["config", "user.email", "zaivern@example.invalid"]);
        run(&["config", "user.name", "zaivern test"]);
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "test"]);
    }
}
