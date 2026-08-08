//! Git パネル (左サイドバー用)。
//!
//! ブランチ / worktree / 変更ファイルを一覧し、**安全で取り消せる操作だけ**を提供する。
//! commit / push / ブランチ削除 / worktree 削除 / merge / rebase は意図的にスコープ外。
//!
//! 同じファイルに [`ReviewPanel`] (GitHub の "Files changed" 風の
//! ローカル変更レビュー) も同居している。そちらは stage / unstage /
//! 変更の破棄を持つが、破棄だけは race パネルと同じ **2 段確認**を必須にしている。
//! app.rs 側の配線は `ReviewPanel` の直前のコメントに書いてある。
//!
//! 設計上の要点:
//! - 一覧の取得 (`git branch` 等) は TTL 付きキャッシュ + バックグラウンド収集。
//!   `ui()` が毎フレーム `git` を fork することは無い。
//! - checkout / worktree add / fetch といった変更系は **必ず別スレッド**で走らせる。
//!   `git fetch` はネットワーク待ちで数十秒固まりうるため、UI スレッドでは絶対に回さない。
//!   (`src/voice.rs` の `std::thread::Builder` + `mpsc` + `ctx.request_repaint()` に倣う)
//! - git が無い / リポジトリでない場合も panic せず、静かな説明行を出すだけ。
//! - linked worktree (`.git` がファイル) でもそのまま動く。git CLI に判断を任せている。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};

use crate::i18n::{tr, trf};
use crate::theme::Theme;

/// 一覧キャッシュの寿命。切れたら次フレームで再収集を仕込む。
const LIST_TTL: Duration = Duration::from_secs(5);

/// パネルが呼び出し側にお願いしたいこと。
#[derive(Default)]
pub struct GitActions {
    /// このパスをワークスペースとして開いてほしい (worktree を開く操作)
    pub open_path: Option<PathBuf>,
    /// 画面に出したいメッセージ (本文, 成功なら true)
    pub toast: Option<(String, bool)>,
    /// このファイルをエディタで開いてほしい (レビュー画面の「エディタで開く」)。
    /// `open_path` (ワークスペースを切り替える) とは別物なので混ぜないこと。
    pub open_file: Option<PathBuf>,
    /// エージェントへ追いプロンプトとして流したいレビュー内容
    /// (レビュー画面のインラインコメント → 「エージェントに送る」)。
    pub review_prompt: Option<String>,
    /// このコミットの差分を開いてほしい `(リポジトリの toplevel, SHA)`。
    /// 履歴一覧のクリック → 既存の `git::commit_diff` → `panels::commit_diff_ui`。
    pub open_commit: Option<(PathBuf, String)>,
}

// ---------------------------------------------------------------------------
// データモデル
// ---------------------------------------------------------------------------

/// HEAD の状態。detached を「それっぽいブランチ名」に偽装しない。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub enum HeadState {
    /// 通常のブランチ上
    OnBranch(String),
    /// detached HEAD (中身は git の説明文 or リビジョン)
    Detached(String),
    /// まだコミットが無い等で判別できない
    #[default]
    Unknown,
}

/// `git branch --all` の 1 行。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BranchEntry {
    pub name: String,
    /// この worktree の HEAD (`*` マーカー)
    pub current: bool,
    /// 別の worktree でチェックアウト中 (`+` マーカー)
    pub other_worktree: bool,
}

/// `git branch --all` のパース結果。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct BranchList {
    pub local: Vec<BranchEntry>,
    /// リモート追跡ブランチ (`remotes/` は剥がしてある)
    pub remote: Vec<String>,
    pub head: Option<HeadState>,
}

/// `git worktree list --porcelain` の 1 レコード。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: bool,
    /// このワークスペースが今開いている worktree か。収集時に前計算する
    /// (canonicalize は実 FS syscall なので毎フレームの UI では呼ばない)。
    pub current: bool,
}

impl WorktreeEntry {
    /// 一覧に出すブランチ相当のラベル。
    pub fn label(&self) -> String {
        if self.bare {
            return "(bare)".to_string();
        }
        match (&self.branch, self.detached) {
            (Some(b), _) => b.clone(),
            (None, true) => {
                let short: String = self
                    .head
                    .as_deref()
                    .unwrap_or("HEAD")
                    .chars()
                    .take(8)
                    .collect();
                format!("(detached {short})")
            }
            (None, false) => tr("(不明)"),
        }
    }
}

/// `git status --porcelain=v1` の 1 行。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChangeEntry {
    /// XY の 2 文字 (例: " M", "??", "R ")
    pub code: String,
    /// 表示対象のパス (rename なら移動先)
    pub path: String,
    /// rename / copy の移動元
    pub orig: Option<String>,
}

impl ChangeEntry {
    /// 見出しに出す 1 文字。index 側を優先し、無ければ worktree 側。
    pub fn letter(&self) -> char {
        let mut cs = self.code.chars();
        let x = cs.next().unwrap_or(' ');
        let y = cs.next().unwrap_or(' ');
        if x != ' ' && x != '?' {
            x
        } else if x == '?' {
            '?'
        } else if y != ' ' {
            y
        } else {
            '·'
        }
    }

    pub fn untracked(&self) -> bool {
        self.code.starts_with('?')
    }

    /// index に載っているか (= コミット対象)。`??` (未追跡) は載っていない。
    pub fn staged(&self) -> bool {
        let x = self.code.chars().next().unwrap_or(' ');
        x != ' ' && x != '?' && x != '!'
    }
}

/// ステージ済みの件数。コミットボタンの活性はこの数だけで決まる。
pub fn staged_count(changes: &[ChangeEntry]) -> usize {
    changes.iter().filter(|c| c.staged()).count()
}

/// upstream との位置関係。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct UpstreamInfo {
    /// `origin/main` 等。未設定なら None。
    pub name: Option<String>,
    /// upstream に無いローカルのコミット数 (push できる数)。
    pub ahead: usize,
    /// ローカルに無い upstream のコミット数 (pull できる数)。
    pub behind: usize,
    /// リモート名の一覧 (`git remote`)。空ならリモート自体が無い。
    pub remotes: Vec<String>,
}

impl UpstreamInfo {
    /// push の宛先として提案する `(remote, branch)`。
    /// upstream が無く、リモートが 1 つ以上あるときだけ返す。
    pub fn suggest_push_target(&self, head: &HeadState) -> Option<(String, String)> {
        if self.name.is_some() {
            return None;
        }
        let HeadState::OnBranch(b) = head else {
            return None;
        };
        let remote = self
            .remotes
            .iter()
            .find(|r| *r == "origin")
            .or_else(|| self.remotes.first())?;
        Some((remote.clone(), b.clone()))
    }
}

/// 収集済みリポジトリ情報。
#[derive(Clone, Default)]
pub struct RepoInfo {
    pub toplevel: PathBuf,
    pub head: HeadState,
    pub branches: BranchList,
    pub worktrees: Vec<WorktreeEntry>,
    pub changes: Vec<ChangeEntry>,
    /// upstream / ahead / behind / remote 一覧 (push・pull ボタンの判断材料)。
    pub upstream: UpstreamInfo,
    /// 直前のコミットのメッセージ全文 (`--amend` の初期値)。
    pub head_message: Option<String>,
}

/// パネルの表示状態。
#[derive(Clone)]
enum RepoState {
    /// 初回収集がまだ終わっていない
    Loading,
    /// git が無い / リポジトリでない等 (穏やかな説明文)
    Unavailable(String),
    Ready(Box<RepoInfo>),
}

// ---------------------------------------------------------------------------
// git 実行
// ---------------------------------------------------------------------------

/// git 実行の失敗理由。
#[derive(Clone, Debug)]
enum RunErr {
    /// プロセスを起動できなかった (git が入っていない等)
    Spawn(String),
    /// git は動いたが非ゼロ終了。中身は stderr をそのまま。
    Failed(String),
}

impl RunErr {
    fn text(&self) -> &str {
        match self {
            RunErr::Spawn(s) | RunErr::Failed(s) => s,
        }
    }
}

/// `git -C <ws> <args>` を同期実行する。呼ぶ側がスレッドを用意すること。
fn run_git(ws: &Path, args: &[&str]) -> Result<String, RunErr> {
    // color.ui=always な環境でも ANSI エスケープ無しの出力を得る
    // (branch 一覧の "* " マーカー判定やブランチ名検証が壊れないように)
    let out = git_command(ws, args)
        .output()
        .map_err(|e| RunErr::Spawn(e.to_string()))?;
    finish_output(out, args)
}

/// ネットワークを触る git (push / pull / fetch) を待たせずに走らせる上限。
const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);

/// 対話プロンプトを**構造的に**封じる。認証を聞かれたら待たずに失敗させる。
///
/// - `GIT_TERMINAL_PROMPT=0`: git 自身のユーザー名 / パスワード入力を止める
/// - `GIT_ASKPASS` / `SSH_ASKPASS` を空にし `SSH_ASKPASS_REQUIRE=never`:
///   GUI のパスワードダイアログ経路も塞ぐ
/// - `GIT_SSH_COMMAND`: ssh を `BatchMode=yes` にして鍵パスフレーズ待ちを止める
/// - `GIT_LFS_SKIP_SMUDGE=1`: lfs のフィルタが端末を掴んで止まるのを避ける
fn apply_noninteractive_env(c: &mut Command) {
    c.env("GIT_TERMINAL_PROMPT", "0");
    c.env("GIT_ASKPASS", "");
    c.env("SSH_ASKPASS", "");
    c.env("SSH_ASKPASS_REQUIRE", "never");
    c.env("GIT_LFS_SKIP_SMUDGE", "1");
    c.env(
        "GIT_SSH_COMMAND",
        "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new",
    );
}

/// `git -C <ws> <args>` の共通土台。`color.ui=false` と OS 差分をここへ寄せる。
fn git_command(ws: &Path, args: &[&str]) -> Command {
    let mut c = Command::new("git");
    c.arg("-c")
        .arg("color.ui=false")
        .arg("-C")
        .arg(ws)
        .args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // GUI アプリからコンソール窓を出さない
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// 出力を `RunErr` へ畳む共通処理。
fn finish_output(out: std::process::Output, args: &[&str]) -> Result<String, RunErr> {
    if !out.status.success() {
        let err = crate::textenc::decode_output(&out.stderr)
            .trim()
            .to_string();
        return Err(RunErr::Failed(if err.is_empty() {
            trf("git {args} が失敗しました", &[("args", args.join(" "))])
        } else {
            err
        }));
    }
    Ok(crate::textenc::decode_output(&out.stdout))
}

/// 標準入力へパッチを流し込む `git`。ハンク単位のステージで使う。
fn run_git_stdin(ws: &Path, args: &[&str], input: &str) -> Result<String, RunErr> {
    use std::io::Write;
    let mut c = git_command(ws, args);
    c.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = c.spawn().map_err(|e| RunErr::Spawn(e.to_string()))?;
    if let Some(mut si) = child.stdin.take() {
        // パイプが先に閉じても (git が読まずに終了) 失敗にはしない。
        // 本当の理由は下の終了コード + stderr が持っている。
        let _ = si.write_all(input.as_bytes());
    }
    let out = child
        .wait_with_output()
        .map_err(|e| RunErr::Spawn(e.to_string()))?;
    finish_output(out, args)
}

/// ネットワークを触る `git`。**非対話を強制**し、上限時間で
/// プロセスツリーごと落とす (認証待ちでスレッドを永久に眠らせない)。
fn run_git_network(ws: &Path, args: &[&str]) -> Result<String, RunErr> {
    let mut c = git_command(ws, args);
    apply_noninteractive_env(&mut c);
    c.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 子を独立したプロセスグループにする。こうしないと kill_tree が
        // 孫 (ssh / credential helper) を取り逃がす。
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let mut child = c.spawn().map_err(|e| RunErr::Spawn(e.to_string()))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => return Err(RunErr::Spawn(e.to_string())),
        }
        if started.elapsed() >= NETWORK_TIMEOUT {
            // **まだ生きている**ことを try_wait で確かめた上で撃つ
            // (wait 済みの PID へ撃つと無関係なプロセスを巻き添えにする)。
            crate::procx::kill_tree(child.id());
            let _ = child.wait();
            return Err(RunErr::Failed(tr(
                "時間内に終わりませんでした。認証が要るならターミナルで実行してください",
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child
        .wait_with_output()
        .map_err(|e| RunErr::Spawn(e.to_string()))?;
    finish_output(out, args)
}

/// ワークスペースの git 情報をまとめて集める (バックグラウンドスレッドで呼ぶ)。
fn collect(ws: &Path) -> RepoState {
    let toplevel = match run_git(ws, &["rev-parse", "--show-toplevel"]) {
        Ok(s) => PathBuf::from(s.trim()),
        Err(RunErr::Spawn(_)) => {
            return RepoState::Unavailable(tr("git コマンドが見つかりません"));
        }
        Err(RunErr::Failed(_)) => {
            return RepoState::Unavailable(tr("ここは git リポジトリではありません"));
        }
    };

    let branches = run_git(ws, &["branch", "--all"])
        .map(|s| parse_branch_list(&s))
        .unwrap_or_default();

    // detached 判定は locale に依存しないよう symbolic-ref を正とする。
    let head = match run_git(ws, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        Ok(s) if !s.trim().is_empty() => HeadState::OnBranch(s.trim().to_string()),
        _ => match branches.head.clone() {
            Some(HeadState::Detached(d)) => HeadState::Detached(d),
            _ => match run_git(ws, &["rev-parse", "--short", "HEAD"]) {
                Ok(s) if !s.trim().is_empty() => HeadState::Detached(s.trim().to_string()),
                _ => HeadState::Unknown,
            },
        },
    };

    let mut worktrees = run_git(ws, &["worktree", "list", "--porcelain"])
        .map(|s| parse_worktree_porcelain(&s))
        .unwrap_or_default();
    // 「今ここ」の worktree をこのバックグラウンドスレッドで前計算しておく
    for w in &mut worktrees {
        w.current = same_path(&w.path, &toplevel);
    }

    let changes = run_git(ws, &["status", "--porcelain=v1"])
        .map(|s| parse_status_porcelain(&s))
        .unwrap_or_default();

    // upstream は「無い」が正常系なので失敗を静かに畳む。
    let upstream_name = run_git(
        ws,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty());
    let (ahead, behind) = match &upstream_name {
        Some(_) => run_git(ws, &["rev-list", "--left-right", "--count", "@{u}...HEAD"])
            .ok()
            .and_then(|s| parse_left_right_count(&s))
            .unwrap_or((0, 0)),
        None => (0, 0),
    };
    let remotes = run_git(ws, &["remote"])
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // %B は生のメッセージ全文。改行も引用符もそのまま出る。
    let head_message = run_git(ws, &["log", "-1", "--no-color", "--pretty=format:%B"])
        .ok()
        .map(|s| s.trim_end_matches('\n').to_string())
        .filter(|s| !s.trim().is_empty());

    RepoState::Ready(Box::new(RepoInfo {
        toplevel,
        head,
        branches,
        worktrees,
        changes,
        upstream: UpstreamInfo {
            name: upstream_name,
            ahead,
            behind,
            remotes,
        },
        head_message,
    }))
}

/// `git rev-list --left-right --count @{u}...HEAD` の出力 → `(ahead, behind)`。
///
/// 出力は `<behind>\t<ahead>` (左 = upstream 側にしか無い数)。
/// **ロケール非依存** — 数字とタブしか出ない形式を選んでいる。
pub fn parse_left_right_count(out: &str) -> Option<(usize, usize)> {
    let line = out.lines().find(|l| !l.trim().is_empty())?;
    let mut it = line.split_whitespace();
    let left: usize = it.next()?.parse().ok()?;
    let right: usize = it.next()?.parse().ok()?;
    Some((right, left))
}

// ---------------------------------------------------------------------------
// 純粋なパース関数 (ここだけをテストする)
// ---------------------------------------------------------------------------

/// `git worktree list --porcelain` をパースする。
///
/// 空行区切りのレコード形式。各レコードは `worktree <path>` で始まり、
/// `HEAD <sha>` / `branch refs/heads/<name>` / `detached` / `bare` / `locked` が続く。
pub fn parse_worktree_porcelain(output: &str) -> Vec<WorktreeEntry> {
    let mut out = Vec::new();
    let mut cur: Option<WorktreeEntry> = None;

    for line in output.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() {
            // レコード区切り
            if let Some(w) = cur.take() {
                out.push(w);
            }
            continue;
        }
        let (key, val) = match line.split_once(' ') {
            Some((k, v)) => (k, v.trim()),
            None => (line, ""),
        };
        match key {
            "worktree" => {
                if let Some(w) = cur.take() {
                    out.push(w);
                }
                cur = Some(WorktreeEntry {
                    path: PathBuf::from(val),
                    head: None,
                    branch: None,
                    detached: false,
                    bare: false,
                    locked: false,
                    current: false,
                });
            }
            "HEAD" => {
                if let Some(w) = cur.as_mut() {
                    w.head = Some(val.to_string());
                }
            }
            "branch" => {
                if let Some(w) = cur.as_mut() {
                    // refs/heads/foo -> foo (それ以外の ref はそのまま)
                    w.branch = Some(val.strip_prefix("refs/heads/").unwrap_or(val).to_string());
                }
            }
            "detached" => {
                if let Some(w) = cur.as_mut() {
                    w.detached = true;
                }
            }
            "bare" => {
                if let Some(w) = cur.as_mut() {
                    w.bare = true;
                }
            }
            "locked" => {
                if let Some(w) = cur.as_mut() {
                    w.locked = true;
                }
            }
            _ => {}
        }
    }
    if let Some(w) = cur.take() {
        out.push(w);
    }
    out
}

/// `git branch --all` の出力をパースする。
///
/// 行頭 2 文字がマーカー: `"* "` = この worktree の HEAD、`"+ "` = 別 worktree で使用中。
/// detached HEAD は `* (HEAD detached at abc1234)` のように括弧付きで出る。
pub fn parse_branch_list(output: &str) -> BranchList {
    let mut list = BranchList::default();

    for raw in output.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let (current, other_worktree, rest) = if let Some(r) = line.strip_prefix("* ") {
            (true, false, r.trim())
        } else if let Some(r) = line.strip_prefix("+ ") {
            (false, true, r.trim())
        } else {
            (false, false, line.trim())
        };
        if rest.is_empty() {
            continue;
        }

        // detached HEAD 行: "(HEAD detached at abc1234)" / "(no branch)"
        if rest.starts_with('(') && rest.ends_with(')') {
            if current {
                let inner = rest[1..rest.len() - 1].trim().to_string();
                list.head = Some(HeadState::Detached(inner));
            }
            continue;
        }

        if let Some(remote) = rest.strip_prefix("remotes/") {
            // "origin/HEAD -> origin/main" のシンボリックリンク行は出さない
            if remote.contains(" -> ") {
                continue;
            }
            list.remote.push(remote.to_string());
            continue;
        }

        // 稀に "main -> other" 形式が来ても名前側だけ拾う
        let name = rest.split(" -> ").next().unwrap_or(rest).trim().to_string();
        if name.is_empty() {
            continue;
        }
        if current {
            list.head = Some(HeadState::OnBranch(name.clone()));
        }
        list.local.push(BranchEntry {
            name,
            current,
            other_worktree,
        });
    }
    list
}

/// `git status --porcelain=v1` の出力をパースする。
///
/// 形式は `XY <path>`、rename / copy は `XY <orig> -> <path>`。
pub fn parse_status_porcelain(output: &str) -> Vec<ChangeEntry> {
    let mut out = Vec::new();
    for line in output.lines() {
        // "XY " の 3 バイト + パス。マーカーは ASCII 固定。
        // 行頭がマルチバイト文字だと [..2] / [3..] が文字の内部を指して
        // パニックするため、両方の境界を検査してから切り出す。
        if line.len() < 4 || !line.is_char_boundary(2) || !line.is_char_boundary(3) {
            continue;
        }
        let code = line[..2].to_string();
        let rest = line[3..].trim();
        if rest.is_empty() {
            continue;
        }
        let (orig, path) = match rest.split_once(" -> ") {
            Some((o, p)) => (Some(o.trim().to_string()), p.trim().to_string()),
            None => (None, rest.to_string()),
        };
        out.push(ChangeEntry { code, path, orig });
    }
    out
}

// ---------------------------------------------------------------------------
// 入力の検証
// ---------------------------------------------------------------------------

/// ブランチ名を検証して trim 済みの名前を返す。
///
/// 空 / `-` 始まり (git のオプションと誤認される) を必ず弾く。
pub fn validate_branch_name(input: &str) -> Result<String, String> {
    let n = input.trim();
    if n.is_empty() {
        return Err(tr("名前を入力してください"));
    }
    if n.starts_with('-') {
        return Err(tr("名前を - で始めることはできません"));
    }
    if n.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(tr("名前に空白や制御文字は使えません"));
    }
    if n.contains("..") || n.contains("@{") {
        return Err(tr("名前に .. や @{ は使えません"));
    }
    if n.chars()
        .any(|c| matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        return Err(tr("名前に ~ ^ : ? * [ \\ は使えません"));
    }
    if n.starts_with('/') || n.ends_with('/') || n.ends_with(".lock") {
        return Err(tr("名前の先頭/末尾が不正です"));
    }
    Ok(n.to_string())
}

/// worktree の入力を検証する。ブランチ名よりは緩く、絶対パスも許す。
pub fn validate_worktree_input(input: &str) -> Result<String, String> {
    let n = input.trim();
    if n.is_empty() {
        return Err(tr("worktree 名を入力してください"));
    }
    if n.starts_with('-') {
        return Err(tr("名前を - で始めることはできません"));
    }
    if n.chars().any(char::is_control) {
        return Err(tr("名前に制御文字は使えません"));
    }
    if n.contains("..") {
        return Err(tr("名前に .. は使えません"));
    }
    Ok(n.to_string())
}

/// worktree の既定の置き場所。リポジトリ本体を汚さないよう隣に並べる。
///
/// `<main の親>/<リポジトリ名>-worktrees/<name>`
pub fn default_worktree_base(main_worktree: &Path) -> PathBuf {
    let repo = main_worktree
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let parent = main_worktree.parent().unwrap_or(main_worktree);
    parent.join(format!("{repo}-worktrees"))
}

/// 入力から (作成先パス, 新規ブランチ名の候補) を決める。
pub fn resolve_worktree_target(
    main_worktree: &Path,
    input: &str,
) -> Result<(PathBuf, String), String> {
    let n = validate_worktree_input(input)?;
    let p = Path::new(&n);
    let path = if p.is_absolute() {
        p.to_path_buf()
    } else {
        default_worktree_base(main_worktree).join(&n)
    };
    let leaf = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let branch = validate_branch_name(&leaf)?;
    Ok((path, branch))
}

// ---------------------------------------------------------------------------
// commit / push / pull / log の引数表 (純関数 — ここだけをテストする)
// ---------------------------------------------------------------------------

/// `git commit` の引数表。
///
/// **メッセージはシェルを通さず引数配列の 1 要素として渡す**ので、
/// `"` も改行も絵文字も日本語もそのまま通る (クォート処理は存在しない)。
/// `--cleanup=whitespace` を明示するのは、ユーザーの `commit.cleanup` 設定に
/// 引きずられて `#` 始まりの行が消えるのを防ぐため。
pub fn commit_args(message: &str, amend: bool) -> Vec<String> {
    let mut a: Vec<String> = vec!["commit".into(), "--cleanup=whitespace".into()];
    if amend {
        a.push("--amend".into());
    }
    a.push("--message".into());
    a.push(message.to_string());
    a
}

/// コミットできない理由 (無ければ None)。ボタンの活性はこれ 1 本で決まる。
pub fn commit_blocker(staged: usize, amend: bool, message: &str) -> Option<String> {
    if message.trim().is_empty() {
        return Some(tr("コミットメッセージを入力してください"));
    }
    // amend は「直前のコミットを書き換える」ので、ステージ 0 件でも意味がある。
    if staged == 0 && !amend {
        return Some(tr("ステージ済みの変更がありません"));
    }
    None
}

/// `git push` の引数表。**force は一切生成しない** (このアプリの方針)。
///
/// `set_upstream` はユーザーが明示的に確認したときだけ true にすること。
pub fn push_args(remote: &str, branch: &str, set_upstream: bool) -> Result<Vec<String>, String> {
    let r = remote.trim();
    let b = branch.trim();
    if r.is_empty() || b.is_empty() {
        return Err(tr("push 先のリモートとブランチが決まりません"));
    }
    if r.starts_with('-') || b.starts_with('-') {
        return Err(tr("名前を - で始めることはできません"));
    }
    let mut a: Vec<String> = vec!["push".into()];
    if set_upstream {
        a.push("--set-upstream".into());
    }
    a.push("--".into());
    a.push(r.to_string());
    a.push(b.to_string());
    Ok(a)
}

/// `git pull` の引数表。**早送りできるときだけ**取り込む
/// (勝手にマージコミットや rebase を作らない)。
pub fn pull_args() -> Vec<String> {
    vec!["pull".into(), "--ff-only".into()]
}

/// 履歴 1 ページの件数。
pub const HISTORY_PAGE: usize = 30;

/// `git log` の引数表。区切りは **NUL (`%x00`)** 固定。
///
/// コミットメッセージには改行もタブも `|` も入るので、行や記号で区切ると
/// 必ず壊れる。NUL だけはメッセージに入り得ない。
pub fn log_args(skip: usize, count: usize) -> Vec<String> {
    vec![
        "log".into(),
        "--no-color".into(),
        "-z".into(),
        format!("--skip={skip}"),
        format!("--max-count={count}"),
        "--pretty=format:%H%x00%h%x00%an%x00%at%x00%s".into(),
    ]
}

/// 履歴 1 行ぶん。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CommitEntry {
    pub sha: String,
    pub short: String,
    pub author: String,
    /// author date (unix epoch 秒)
    pub time: i64,
    /// 件名 (`%s`)。git が 1 行へ畳んでくれるので改行は入らない。
    pub subject: String,
}

/// [`log_args`] の出力をパースする。
///
/// フィールド数は 5 固定なので、5 個ずつ切り出すだけ。区切りが NUL なので
/// **メッセージに改行・タブ・引用符・絵文字が入っても列がずれない**。
pub fn parse_log_z(out: &str) -> Vec<CommitEntry> {
    let fields: Vec<&str> = out.split('\0').collect();
    let mut v = Vec::new();
    for c in fields.chunks(5) {
        if c.len() < 5 {
            break;
        }
        let sha = c[0].trim_matches('\n');
        if sha.is_empty() {
            continue;
        }
        v.push(CommitEntry {
            sha: sha.to_string(),
            short: c[1].to_string(),
            author: c[2].to_string(),
            time: c[3].trim().parse().unwrap_or(0),
            subject: c[4].to_string(),
        });
    }
    v
}

// ---------------------------------------------------------------------------
// パネル本体
// ---------------------------------------------------------------------------

/// 走らせたい変更系ジョブ。UI 描画中に決め、描画後に spawn する。
enum Job {
    Checkout(String),
    NewBranch(String),
    WorktreeAdd {
        path: PathBuf,
        branch: Option<String>,
    },
    Fetch,
    /// コミット (`--amend` 込み)
    Commit {
        message: String,
        amend: bool,
    },
    /// push。`set_upstream` はユーザーが確認したときだけ true。
    Push {
        remote: String,
        branch: String,
        set_upstream: bool,
    },
    /// pull (`--ff-only`)
    Pull,
}

pub struct GitPanel {
    workspace: PathBuf,
    state: RepoState,
    /// 最後に収集を仕込んだ時刻。None なら即再収集。
    last_refresh: Option<Instant>,
    /// 走行中の一覧収集
    pending: Option<Receiver<RepoState>>,
    /// 走行中の変更系ジョブ (同時に 1 つだけ)
    job: Option<Receiver<(String, bool)>>,
    /// 走行中ジョブの表示名
    job_label: String,
    new_branch_input: String,
    worktree_input: String,
    worktree_new_branch: bool,
    show_remote: bool,
    /// コミットメッセージ (1 行目 = 件名 / 空行 / 本文)
    commit_msg: String,
    /// `--amend` (直前のコミットを書き換える)
    amend: bool,
    /// amend を入れた瞬間に直前のメッセージを 1 度だけ読み込むための札。
    amend_loaded: bool,
    /// upstream 未設定のまま push を押したときの確認 `(remote, branch)`。
    confirm_upstream: Option<(String, String)>,
    /// 取得済みの履歴 (ページングで後ろへ伸びる)
    history: Vec<CommitEntry>,
    /// まだ続きがあるか (最後のページが満杯だったか)
    history_more: bool,
    /// 走行中の履歴取得
    history_rx: Option<Receiver<(Vec<CommitEntry>, bool)>>,
    /// 履歴セクションがこのフレームで開かれていたか (開くまで git log を撃たない)
    history_open: bool,
    /// 走行中のジョブが成功したらメッセージ欄を空にするか (コミットのときだけ)
    job_clears_commit: bool,
}

impl GitPanel {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            state: RepoState::Loading,
            last_refresh: None,
            pending: None,
            job: None,
            job_label: String::new(),
            new_branch_input: String::new(),
            worktree_input: String::new(),
            worktree_new_branch: true,
            show_remote: false,
            commit_msg: String::new(),
            amend: false,
            amend_loaded: false,
            confirm_upstream: None,
            history: Vec::new(),
            history_more: false,
            history_rx: None,
            history_open: false,
            job_clears_commit: false,
        }
    }

    /// 履歴を捨てて次に開いたとき取り直させる。
    fn reset_history(&mut self) {
        self.history.clear();
        self.history_more = false;
        // 飛行中の取得は受信口ごと捨てる (旧リポジトリの結果を混ぜない)。
        self.history_rx = None;
    }

    pub fn set_workspace(&mut self, ws: PathBuf) {
        if self.workspace != ws {
            self.workspace = ws;
            self.state = RepoState::Loading;
            // 別リポジトリのメッセージ / 履歴を持ち越さない
            self.commit_msg.clear();
            self.amend = false;
            self.amend_loaded = false;
            self.confirm_upstream = None;
            self.reset_history();
            // 旧ワークスペース向けの飛行中の収集は受信口ごと捨てる。
            // 残すと旧リポジトリの結果が新パネルとして表示され、
            // そのブランチ名で checkout を発行できてしまう。
            self.pending = None;
            self.invalidate();
        }
    }

    /// 次フレームで一覧を取り直させる。
    pub fn invalidate(&mut self) {
        self.last_refresh = None;
    }

    /// 変更系ジョブが走行中か。
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// 毎フレーム呼ばれる。TTL 切れなら情報を取り直し、走っているジョブの完了も回収する。
    pub fn ui(&mut self, ui: &mut egui::Ui, theme: &Theme, actions: &mut GitActions) {
        let ctx = ui.ctx().clone();
        self.poll(actions);
        self.maybe_refresh(&ctx);

        let busy = self.job.is_some();
        // state を一旦持ち出して、描画中も self の入力欄を可変で触れるようにする。
        let state = std::mem::replace(&mut self.state, RepoState::Loading);
        let mut req: Option<Job> = None;

        self.header_ui(ui, theme, busy);

        match &state {
            RepoState::Loading => {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.label(
                        RichText::new(tr("読み込み中…"))
                            .color(theme.text_dim)
                            .small(),
                    );
                });
            }
            RepoState::Unavailable(msg) => {
                ui.label(RichText::new(msg).color(theme.text_dim).small());
            }
            RepoState::Ready(info) => {
                self.head_ui(ui, theme, info);
                ui.separator();
                self.commit_ui(ui, theme, info, busy, &mut req);
                ui.separator();
                self.branches_ui(ui, theme, info, busy, &mut req);
                ui.separator();
                self.worktrees_ui(ui, theme, info, busy, actions, &mut req);
                ui.separator();
                self.changes_ui(ui, theme, info);
                ui.separator();
                self.history_ui(ui, theme, info, actions);
            }
        }

        if busy && !self.job_label.is_empty() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(11.0));
                ui.label(
                    RichText::new(trf("{label} 実行中…", &[("label", self.job_label.clone())]))
                        .color(theme.text_dim)
                        .small(),
                );
            });
        }

        self.state = state;

        // 履歴は**開いたときだけ**取りに行く (巨大リポで開くたびに固まらせない)。
        if self.history_open && self.history.is_empty() && self.history_rx.is_none() {
            self.spawn_history(&ctx, 0);
        }

        if let Some(job) = req {
            self.spawn_job(&ctx, job, actions);
        }
    }

    // -- 各セクション -------------------------------------------------------

    /// コミット導線。メッセージ欄 + [コミット] + amend + push / pull。
    ///
    /// **ここが無いとユーザーは必ずターミナルへ落ちる**ので、
    /// 折りたたまず常に見える位置 (HEAD の直下) に置く。
    fn commit_ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        info: &RepoInfo,
        busy: bool,
        req: &mut Option<Job>,
    ) {
        let staged = staged_count(&info.changes);
        // amend に入れた瞬間だけ直前のメッセージを流し込む (以後は編集を尊重)。
        if self.amend && !self.amend_loaded {
            self.amend_loaded = true;
            if self.commit_msg.trim().is_empty() {
                if let Some(m) = &info.head_message {
                    self.commit_msg = m.clone();
                }
            }
        }
        if !self.amend {
            self.amend_loaded = false;
        }

        let avail = ui.available_width();
        ui.add(
            egui::TextEdit::multiline(&mut self.commit_msg)
                .id_salt("zv-git-commit-msg")
                .desired_rows(3)
                .desired_width(avail)
                .hint_text(tr("1 行目 = 件名 / 2 行目は空けて本文")),
        );

        let blocker = commit_blocker(staged, self.amend, &self.commit_msg);
        let up = &info.upstream;
        let can_pull = up.name.is_some();
        let push_target = up.suggest_push_target(&info.head);
        let can_push = up.name.is_some() || push_target.is_some();

        // ボタンの並びは可用幅で決める (純関数 → テーブルテストで固定)。
        let commit_label = if self.amend {
            tr("直前を修正")
        } else {
            tr("コミット")
        };
        let push_label = if up.ahead > 0 {
            trf("push ({n})", &[("n", up.ahead.to_string())])
        } else {
            tr("push")
        };
        let pull_label = if up.behind > 0 {
            trf("pull ({n})", &[("n", up.behind.to_string())])
        } else {
            tr("pull")
        };
        let full = [
            commit_label.clone(),
            tr("直前を修正 (amend)"),
            push_label.clone(),
            pull_label.clone(),
            tr("fetch"),
        ];
        let icons = [
            "✔".to_string(),
            "✎".into(),
            "⬆".into(),
            "⬇".into(),
            "⟳".into(),
        ];
        let char_w = ui.fonts(|f| f.glyph_width(&egui::FontId::proportional(12.0), '0'));
        let plan = crate::diff::plan_button_bar(avail, &full, &icons, char_w);
        let text = |i: usize| -> String {
            if plan.icon_only {
                icons[i].clone()
            } else {
                full[i].clone()
            }
        };

        for row in &plan.rows {
            ui.horizontal_wrapped(|ui| {
                for &i in row {
                    match i {
                        0 => {
                            let r = ui
                                .add_enabled(
                                    !busy && blocker.is_none(),
                                    egui::Button::new(text(0)).small(),
                                )
                                // 無効なときは**理由**をホバーで出す (押せない訳が分かる)
                                .on_hover_text(match &blocker {
                                    Some(why) => why.clone(),
                                    None => commit_label.clone(),
                                });
                            if r.clicked() {
                                *req = Some(Job::Commit {
                                    message: self.commit_msg.clone(),
                                    amend: self.amend,
                                });
                            }
                        }
                        1 => {
                            if ui
                                .selectable_label(self.amend, text(1))
                                .on_hover_text(tr(
                                    "直前のコミットを作り直す (まだ push していないときだけ使う)",
                                ))
                                .clicked()
                            {
                                self.amend = !self.amend;
                            }
                        }
                        2 => {
                            if ui
                                .add_enabled(!busy && can_push, egui::Button::new(text(2)).small())
                                .on_hover_text(match &up.name {
                                    Some(n) => trf("{n} へ push", &[("n", n.clone())]),
                                    None => tr("upstream が未設定です"),
                                })
                                .clicked()
                            {
                                match (&up.name, &push_target) {
                                    // upstream があるならそのまま押し出す
                                    (Some(_), _) => {
                                        if let HeadState::OnBranch(b) = &info.head {
                                            let remote = up
                                                .name
                                                .as_deref()
                                                .and_then(|n| n.split_once('/'))
                                                .map(|(r, _)| r.to_string())
                                                .unwrap_or_else(|| "origin".to_string());
                                            *req = Some(Job::Push {
                                                remote,
                                                branch: b.clone(),
                                                set_upstream: false,
                                            });
                                        }
                                    }
                                    // 無いときは**勝手に --set-upstream しない**。確認を挟む。
                                    (None, Some(t)) => self.confirm_upstream = Some(t.clone()),
                                    (None, None) => {}
                                }
                            }
                        }
                        3 => {
                            if ui
                                .add_enabled(!busy && can_pull, egui::Button::new(text(3)).small())
                                .on_hover_text(tr("早送りできるときだけ取り込む (--ff-only)"))
                                .clicked()
                            {
                                *req = Some(Job::Pull);
                            }
                        }
                        _ => {
                            if ui
                                .add_enabled(!busy, egui::Button::new(text(4)).small())
                                .on_hover_text(tr("リモートの情報を取り直す"))
                                .clicked()
                            {
                                *req = Some(Job::Fetch);
                            }
                        }
                    }
                }
            });
        }

        // upstream 未設定の確認 (押した本人にだけ出る 1 行)。
        if let Some((remote, branch)) = self.confirm_upstream.clone() {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(trf(
                        "upstream 未設定です。{r}/{b} を upstream にしますか?",
                        &[("r", remote.clone()), ("b", branch.clone())],
                    ))
                    .color(theme.warn)
                    .small(),
                );
                if ui.small_button(tr("設定して push")).clicked() {
                    *req = Some(Job::Push {
                        remote: remote.clone(),
                        branch: branch.clone(),
                        set_upstream: true,
                    });
                    self.confirm_upstream = None;
                }
                if ui.small_button(tr("やめる")).clicked() {
                    self.confirm_upstream = None;
                }
            });
        }

        // 「押せない理由」を必ず 1 行で見せる (無効なボタンだけ置いて黙らない)。
        if let Some(reason) = &blocker {
            ui.label(RichText::new(reason).color(theme.text_dim).small());
        } else if staged > 0 {
            ui.label(
                RichText::new(trf("ステージ済み {n} 件", &[("n", staged.to_string())]))
                    .color(theme.text_dim)
                    .small(),
            );
        }
    }

    /// コミット履歴。**開いたときに初回 N 件**、続きはボタンで足す。
    fn history_ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        info: &RepoInfo,
        actions: &mut GitActions,
    ) {
        let mut open = false;
        let mut load_more = false;
        let title = if self.history.is_empty() {
            tr("履歴")
        } else {
            trf("履歴 ({n})", &[("n", self.history.len().to_string())])
        };
        egui::CollapsingHeader::new(RichText::new(title).color(theme.text).small())
            .id_salt("zv_git_history")
            .default_open(false)
            .show(ui, |ui| {
                open = true;
                if self.history.is_empty() {
                    let msg = if self.history_rx.is_some() {
                        tr("読み込み中…")
                    } else {
                        tr("コミットがありません")
                    };
                    ui.label(RichText::new(msg).color(theme.text_dim).small());
                    return;
                }
                let now = crate::git::unix_now();
                for c in &self.history {
                    // 素の Button / SelectableLabel は自動採番なので
                    // 同じ見た目が並んでも ID は衝突しない (push_id 不要)。
                    ui.horizontal(|ui| {
                        let rel = crate::git::relative_time(c.time, now);
                        let resp = ui
                            .selectable_label(
                                false,
                                RichText::new(&c.short)
                                    .monospace()
                                    .color(theme.accent)
                                    .small(),
                            )
                            .on_hover_text(trf(
                                "{s}\n{a} · {t}\n\n{m}",
                                &[
                                    ("s", c.sha.clone()),
                                    ("a", c.author.clone()),
                                    ("t", rel.clone()),
                                    ("m", c.subject.clone()),
                                ],
                            ));
                        if resp.clicked() {
                            actions.open_commit = Some((info.toplevel.clone(), c.sha.clone()));
                        }
                        ui.add(
                            egui::Label::new(RichText::new(&c.subject).color(theme.text).small())
                                .truncate(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(rel).color(theme.text_dim).small());
                        });
                    });
                }
                if self.history_rx.is_some() {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(11.0));
                        ui.label(
                            RichText::new(tr("読み込み中…"))
                                .color(theme.text_dim)
                                .small(),
                        );
                    });
                } else if self.history_more && ui.small_button(tr("続きを読む")).clicked() {
                    load_more = true;
                }
            });
        self.history_open = open;
        if load_more {
            let ctx = ui.ctx().clone();
            self.spawn_history(&ctx, self.history.len());
        }
    }

    fn header_ui(&mut self, ui: &mut egui::Ui, theme: &Theme, busy: bool) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Git").strong().color(theme.text));
            if self.pending.is_some() {
                ui.add(egui::Spinner::new().size(11.0));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(!busy, egui::Button::new("⟳").small())
                    .on_hover_text(tr("リフレッシュ"))
                    .clicked()
                {
                    self.invalidate();
                }
            });
        });
    }

    fn head_ui(&self, ui: &mut egui::Ui, theme: &Theme, info: &RepoInfo) {
        ui.horizontal_wrapped(|ui| match &info.head {
            HeadState::OnBranch(b) => {
                ui.label(RichText::new("⎇").color(theme.accent));
                ui.label(RichText::new(b).strong().color(theme.accent));
            }
            HeadState::Detached(d) => {
                ui.label(RichText::new("⚠").color(theme.warn));
                ui.label(
                    RichText::new(format!("detached HEAD ({d})"))
                        .strong()
                        .color(theme.warn),
                )
                .on_hover_text(tr(
                    "ブランチから外れています。作業前にブランチを作るか切り替えてください",
                ));
            }
            HeadState::Unknown => {
                ui.label(
                    RichText::new(tr("HEAD 不明 (まだコミットがないかもしれません)"))
                        .color(theme.text_dim)
                        .small(),
                );
            }
        });
    }

    fn branches_ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        info: &RepoInfo,
        busy: bool,
        req: &mut Option<Job>,
    ) {
        egui::CollapsingHeader::new(
            RichText::new(trf(
                "ブランチ ({n})",
                &[("n", info.branches.local.len().to_string())],
            ))
            .color(theme.text)
            .small(),
        )
        .id_salt("zv_git_branches")
        .default_open(true)
        .show(ui, |ui| {
            for b in &info.branches.local {
                ui.horizontal(|ui| {
                    let mark = if b.current {
                        "●"
                    } else if b.other_worktree {
                        "◇"
                    } else {
                        " "
                    };
                    let color = if b.current {
                        theme.accent
                    } else {
                        theme.text_dim
                    };
                    ui.label(RichText::new(mark).color(color).small());
                    let label = RichText::new(&b.name)
                        .color(if b.current { theme.accent } else { theme.text })
                        .small();
                    let resp =
                        ui.add_enabled(!busy && !b.current, egui::Button::new(label).frame(false));
                    let resp = if b.current {
                        resp.on_hover_text(tr("現在のブランチ"))
                    } else if b.other_worktree {
                        resp.on_hover_text(tr("別の worktree で使用中。切替は git が判断します"))
                    } else {
                        resp.on_hover_text(tr("クリックで切り替え (git checkout)"))
                    };
                    if resp.clicked() {
                        *req = Some(Job::Checkout(b.name.clone()));
                    }
                });
            }
            if info.branches.local.is_empty() {
                ui.label(
                    RichText::new(tr("ローカルブランチがありません"))
                        .color(theme.text_dim)
                        .small(),
                );
            }

            // 新規ブランチ
            ui.horizontal(|ui| {
                let te = egui::TextEdit::singleline(&mut self.new_branch_input)
                    .desired_width(f32::INFINITY)
                    .hint_text(tr("新しいブランチ名"));
                let resp = ui.add_enabled(!busy, te);
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter && !self.new_branch_input.trim().is_empty() {
                    *req = Some(Job::NewBranch(self.new_branch_input.clone()));
                }
            });
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !busy && !self.new_branch_input.trim().is_empty(),
                        egui::Button::new(RichText::new(tr("＋ ブランチ作成")).small()),
                    )
                    .on_hover_text("git checkout -b")
                    .clicked()
                {
                    *req = Some(Job::NewBranch(self.new_branch_input.clone()));
                }
                if ui
                    .add_enabled(!busy, egui::Button::new(RichText::new("⇩ fetch").small()))
                    .on_hover_text("git fetch --all --prune")
                    .clicked()
                {
                    *req = Some(Job::Fetch);
                }
            });

            if !info.branches.remote.is_empty() {
                egui::CollapsingHeader::new(
                    RichText::new(trf(
                        "リモート追跡 ({n})",
                        &[("n", info.branches.remote.len().to_string())],
                    ))
                    .color(theme.text_dim)
                    .small(),
                )
                .id_salt("zv_git_remote_branches")
                .default_open(false)
                .show(ui, |ui| {
                    for r in &info.branches.remote {
                        ui.label(RichText::new(r).color(theme.text_dim).small());
                    }
                });
            }
        });
    }

    fn worktrees_ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        info: &RepoInfo,
        busy: bool,
        actions: &mut GitActions,
        req: &mut Option<Job>,
    ) {
        egui::CollapsingHeader::new(
            RichText::new(format!("worktree ({})", info.worktrees.len()))
                .color(theme.text)
                .small(),
        )
        .id_salt("zv_git_worktrees")
        .default_open(true)
        .show(ui, |ui| {
            for w in &info.worktrees {
                let is_current = w.current;
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if is_current { "●" } else { "○" })
                            .color(if is_current {
                                theme.accent
                            } else {
                                theme.text_dim
                            })
                            .small(),
                    );
                    let name = w
                        .path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| w.path.display().to_string());
                    let color = if is_current { theme.accent } else { theme.text };
                    ui.label(RichText::new(name).color(color).small())
                        .on_hover_text(w.path.display().to_string());
                    ui.label(RichText::new(w.label()).color(theme.text_dim).small());
                    if w.locked {
                        ui.label(RichText::new("🔒").small());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(
                                !is_current && !w.bare,
                                egui::Button::new(RichText::new(tr("開く")).small()),
                            )
                            .on_hover_text(tr("この worktree をワークスペースとして開く"))
                            .clicked()
                        {
                            actions.open_path = Some(w.path.clone());
                        }
                    });
                });
            }

            // worktree 作成
            ui.horizontal(|ui| {
                let te = egui::TextEdit::singleline(&mut self.worktree_input)
                    .desired_width(f32::INFINITY)
                    .hint_text(tr("新しい worktree 名 / 絶対パス"));
                let _ = ui.add_enabled(!busy, te);
            });
            ui.checkbox(
                &mut self.worktree_new_branch,
                RichText::new(tr("同名のブランチも作る")).small(),
            );

            // 作成先プレビュー (どこに出来るかを隠さない)
            if !self.worktree_input.trim().is_empty() {
                match resolve_worktree_target(main_worktree_of(info), &self.worktree_input) {
                    Ok((p, _)) => {
                        ui.label(
                            RichText::new(format!("→ {}", p.display()))
                                .color(theme.text_dim)
                                .small(),
                        );
                    }
                    Err(e) => {
                        ui.label(RichText::new(e).color(theme.err).small());
                    }
                }
            }

            if ui
                .add_enabled(
                    !busy && !self.worktree_input.trim().is_empty(),
                    egui::Button::new(RichText::new(tr("＋ worktree 作成")).small()),
                )
                .on_hover_text("git worktree add")
                .clicked()
            {
                match resolve_worktree_target(main_worktree_of(info), &self.worktree_input) {
                    Ok((path, branch)) => {
                        *req = Some(Job::WorktreeAdd {
                            path,
                            branch: self.worktree_new_branch.then_some(branch),
                        });
                    }
                    Err(e) => actions.toast = Some((e, false)),
                }
            }
        });
    }

    fn changes_ui(&self, ui: &mut egui::Ui, theme: &Theme, info: &RepoInfo) {
        egui::CollapsingHeader::new(
            RichText::new(trf(
                "変更ファイル ({n})",
                &[("n", info.changes.len().to_string())],
            ))
            .color(theme.text)
            .small(),
        )
        .id_salt("zv_git_changes")
        .default_open(true)
        .show(ui, |ui| {
            if info.changes.is_empty() {
                ui.label(
                    RichText::new(tr("変更はありません"))
                        .color(theme.text_dim)
                        .small(),
                );
                return;
            }
            for c in &info.changes {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(c.letter().to_string())
                            .monospace()
                            .color(change_color(c, theme))
                            .small(),
                    );
                    let text = match &c.orig {
                        Some(o) => format!("{} ← {}", c.path, o),
                        None => c.path.clone(),
                    };
                    ui.label(RichText::new(text).color(theme.text).small())
                        .on_hover_text(format!("{} {}", c.code, c.path));
                });
            }
        });
    }

    // -- ジョブ管理 ---------------------------------------------------------

    /// 走行中の収集 / ジョブの結果を回収する。
    fn poll(&mut self, actions: &mut GitActions) {
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(state) => {
                    self.state = state;
                    self.pending = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.history_rx {
            match rx.try_recv() {
                Ok((mut page, more)) => {
                    self.history.append(&mut page);
                    self.history_more = more;
                    self.history_rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.history_rx = None;
                    self.history_more = false;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.job {
            match rx.try_recv() {
                Ok((text, ok)) => {
                    // コミットが通ったときだけ欄を空にする。失敗したら
                    // 書いた文章を消さない (打ち直しを強いない)。
                    if ok && self.job_clears_commit {
                        self.commit_msg.clear();
                        self.amend = false;
                        self.amend_loaded = false;
                    }
                    actions.toast = Some((text, ok));
                    self.job = None;
                    self.job_label.clear();
                    self.job_clears_commit = false;
                    // 変更が入ったのでキャッシュを捨てる
                    self.reset_history();
                    self.invalidate();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.job = None;
                    self.job_label.clear();
                    self.job_clears_commit = false;
                    self.invalidate();
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    /// 履歴 1 ページをバックグラウンドで取る。
    fn spawn_history(&mut self, ctx: &egui::Context, skip: usize) {
        if self.history_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ws = self.workspace.clone();
        let ctx2 = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("zv-git-log".into())
            .spawn(move || {
                let args = log_args(skip, HISTORY_PAGE);
                let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                let page = run_git(&ws, &argv)
                    .map(|s| parse_log_z(&s))
                    .unwrap_or_default();
                let more = page.len() >= HISTORY_PAGE;
                let _ = tx.send((page, more));
                ctx2.request_repaint();
            });
        if spawned.is_ok() {
            self.history_rx = Some(rx);
        }
    }

    /// TTL 切れなら一覧収集をバックグラウンドで仕込む。
    fn maybe_refresh(&mut self, ctx: &egui::Context) {
        if self.pending.is_some() {
            return;
        }
        if let Some(t) = self.last_refresh {
            if t.elapsed() < LIST_TTL {
                return;
            }
        }
        // 失敗しても時刻は進める (毎フレーム再試行しない)
        self.last_refresh = Some(Instant::now());

        let (tx, rx) = mpsc::channel();
        let ws = self.workspace.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("zv-git-list".into())
            .spawn(move || {
                let state = collect(&ws);
                let _ = tx.send(state);
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => self.pending = Some(rx),
            Err(e) => {
                self.state = RepoState::Unavailable(trf(
                    "git 情報を取得できません: {e}",
                    &[("e", e.to_string())],
                ));
            }
        }
    }

    /// 変更系コマンドを別スレッドで走らせる。UI は絶対にブロックしない。
    fn spawn_job(&mut self, ctx: &egui::Context, job: Job, actions: &mut GitActions) {
        if self.job.is_some() {
            return;
        }
        // 成功時に消すのは、そのジョブが使った入力欄だけ。
        // Fetch や Checkout で入力途中のブランチ名/worktree 名を消さない。
        let (clear_branch, clear_worktree) = match &job {
            Job::NewBranch(_) => (true, false),
            Job::WorktreeAdd { .. } => (false, true),
            _ => (false, false),
        };
        self.job_clears_commit = matches!(job, Job::Commit { .. });
        // ネットワークを触るジョブだけ非対話 + タイムアウトで走らせる。
        let network = matches!(job, Job::Push { .. } | Job::Pull | Job::Fetch);
        let (label, args) = match job {
            Job::Checkout(b) => match validate_branch_name(&b) {
                Ok(b) => (format!("checkout {b}"), vec!["checkout".into(), b]),
                Err(e) => {
                    actions.toast = Some((e, false));
                    return;
                }
            },
            Job::NewBranch(b) => match validate_branch_name(&b) {
                Ok(b) => (
                    trf("ブランチ作成 {b}", &[("b", b.clone())]),
                    vec!["checkout".into(), "-b".into(), b],
                ),
                Err(e) => {
                    actions.toast = Some((e, false));
                    return;
                }
            },
            Job::WorktreeAdd { path, branch } => {
                let p = path.to_string_lossy().into_owned();
                if p.trim().is_empty() || p.starts_with('-') {
                    actions.toast = Some((tr("worktree のパスが不正です"), false));
                    return;
                }
                let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
                if let Some(b) = branch {
                    args.push("-b".into());
                    args.push(b);
                }
                args.push(p.clone());
                (trf("worktree 作成 {p}", &[("p", p)]), args)
            }
            Job::Fetch => (
                "fetch".to_string(),
                vec!["fetch".into(), "--all".into(), "--prune".into()],
            ),
            Job::Commit { message, amend } => {
                // 最終防波堤は「空メッセージ」だけ (ステージ件数の判断は描画側が済ませている)。
                if let Some(e) = commit_blocker(1, amend, &message) {
                    actions.toast = Some((e, false));
                    return;
                }
                let label = if amend {
                    tr("コミット (amend)")
                } else {
                    tr("コミット")
                };
                (label, commit_args(&message, amend))
            }
            Job::Push {
                remote,
                branch,
                set_upstream,
            } => match push_args(&remote, &branch, set_upstream) {
                Ok(a) => (trf("push {r}/{b}", &[("r", remote), ("b", branch)]), a),
                Err(e) => {
                    actions.toast = Some((e, false));
                    return;
                }
            },
            Job::Pull => ("pull".to_string(), pull_args()),
        };

        let (tx, rx) = mpsc::channel();
        let ws = self.workspace.clone();
        let ctx2 = ctx.clone();
        let label2 = label.clone();
        let spawned = std::thread::Builder::new()
            .name("zv-git-job".into())
            .spawn(move || {
                let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                let run = if network {
                    run_git_network(&ws, &argv)
                } else {
                    run_git(&ws, &argv)
                };
                // stderr は加工せずそのまま伝える (git の拒否理由を握り潰さない)
                let msg = match run {
                    Ok(_) => (trf("{label2} 完了", &[("label2", label2.clone())]), true),
                    Err(e) => {
                        let mut t = trf(
                            "{label2} 失敗: {e}",
                            &[("label2", label2.clone()), ("e", e.text().to_string())],
                        );
                        // 対話は構造的に封じてあるので、認証が要るなら
                        // ここでは絶対に通らない。逃げ道を必ず案内する。
                        if network {
                            t.push('\n');
                            t.push_str(&tr("認証が必要な場合はターミナルで実行してください"));
                        }
                        (t, false)
                    }
                };
                let _ = tx.send(msg);
                ctx2.request_repaint();
            });
        match spawned {
            Ok(_) => {
                self.job = Some(rx);
                self.job_label = label;
                // 使った入力欄だけ空にする
                if clear_branch {
                    self.new_branch_input.clear();
                }
                if clear_worktree {
                    self.worktree_input.clear();
                }
            }
            Err(e) => {
                actions.toast = Some((
                    trf("git を起動できません: {e}", &[("e", e.to_string())]),
                    false,
                ));
            }
        }
    }
}

/// worktree 一覧の先頭 = メイン worktree。無ければ toplevel で代用。
fn main_worktree_of(info: &RepoInfo) -> &Path {
    info.worktrees
        .first()
        .map(|w| w.path.as_path())
        .unwrap_or(info.toplevel.as_path())
}

/// パスの同一判定。canonicalize できればそれで、駄目なら素で比べる。
fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn change_color(c: &ChangeEntry, theme: &Theme) -> egui::Color32 {
    match c.letter() {
        '?' => theme.text_dim,
        'A' => theme.ok,
        'D' => theme.err,
        'R' | 'C' => theme.accent,
        'U' => theme.warn,
        _ => theme.warn,
    }
}

// ---------------------------------------------------------------------------
// PR 風のローカル変更レビュー (GitHub の "Files changed" 相当)
// ---------------------------------------------------------------------------
//
// # app.rs 側に必要な配線 (このファイルだけでは画面に出ない)
//
// 1. 状態を 1 つ持つ:   `review: git_panel::ReviewPanel`
//    - 生成:            `git_panel::ReviewPanel::new(workspace.clone())`
//    - ws 切替時:       `self.review.set_workspace(ws.clone());`
// 2. タブ / パネルの登録 (**これが無いと表示されない**):
//    中央タブか右パネルの描画で 1 行、
//    `self.review.ui(ui, &theme, &mut git_actions);`
//    既存の `GitActions` をそのまま使い回せる。
// 3. `GitActions` に足した 2 フィールドを既存の処理へ流す:
//    - `open_file`     → エディタでそのパスを開く (`open_path` とは別物)
//    - `review_prompt` → `agent_input` へ追いプロンプトとして渡す
// 4. 任意: コマンドパレット / メニュー / キーバインドから
//    `ReviewPanel::invalidate()` を叩けば次フレームで再収集する。
//
// git 実行とファイル読みは全てバックグラウンドスレッド + mpsc。
// `ui()` はキャッシュを読むだけで、UI スレッドから git を起動しない。

/// 比較のベース。既定は「作業ツリー vs HEAD」。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum ReviewBase {
    /// 作業ツリー全体 vs HEAD (ステージ済み + 未ステージ + 未追跡)
    #[default]
    Head,
    /// ステージ済みだけ (index vs HEAD)
    Staged,
    /// 未ステージだけ (作業ツリー vs index)
    Unstaged,
    /// 任意のブランチ / コミット (`main`, `origin/main`, SHA, `HEAD~3` …)
    Rev(String),
}

impl ReviewBase {
    pub fn label(&self) -> String {
        match self {
            ReviewBase::Head => tr("作業ツリー vs HEAD"),
            ReviewBase::Staged => tr("ステージ済み (index) vs HEAD"),
            ReviewBase::Unstaged => tr("未ステージ (作業ツリー vs index)"),
            ReviewBase::Rev(r) => trf("作業ツリー vs {r}", &[("r", r.clone())]),
        }
    }

    /// 未追跡ファイルを合成 diff として足すベースか。
    /// index との比較 (Staged) には未追跡は含まれない。
    pub fn includes_untracked(&self) -> bool {
        !matches!(self, ReviewBase::Staged)
    }
}

/// 文脈行数の選択。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ContextLines {
    #[default]
    Three,
    Ten,
    /// ファイル全体 (git に十分大きい値を渡す)
    All,
}

impl ContextLines {
    pub fn value(self) -> u32 {
        match self {
            ContextLines::Three => 3,
            ContextLines::Ten => 10,
            ContextLines::All => 100_000,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            ContextLines::Three => "3",
            ContextLines::Ten => "10",
            ContextLines::All => "全部",
        }
    }
}

/// レビュー全体の diff テキスト上限 (これを超えたら行境界で打ち切る)。
const MAX_REVIEW_DIFF_BYTES: usize = 1_500_000;
/// 未追跡ファイル 1 本を合成 diff にするときの読み込み上限。
const MAX_UNTRACKED_BYTES: usize = 128 * 1024;
/// レビューの再収集 TTL。
const REVIEW_TTL: Duration = Duration::from_secs(5);

/// リビジョン入力の検証。`validate_branch_name` より緩い
/// (`HEAD~3` / `a..b` / `origin/main` を通す) が、
/// **空と `-` 始まり (git のオプションと誤認) と制御文字は必ず弾く**。
pub fn validate_rev(input: &str) -> Result<String, String> {
    let n = input.trim();
    if n.is_empty() {
        return Err(tr("比較先のブランチ / コミットを入力してください"));
    }
    if n.starts_with('-') {
        return Err(tr("名前を - で始めることはできません"));
    }
    if n.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(tr("名前に空白や制御文字は使えません"));
    }
    Ok(n.to_string())
}

/// `git diff` の引数表。純関数なのでテストで表として検証する。
///
/// `--no-color` は `run_git` の `-c color.ui=false` と二重の保険。
/// `-M` はリネーム検出を明示 (古い git でも同じ結果になるように)。
pub fn review_diff_args(base: &ReviewBase, ctx: ContextLines, ignore_ws: bool) -> Vec<String> {
    let mut a: Vec<String> = vec!["diff".into(), "--no-color".into(), "-M".into()];
    a.push(format!("--unified={}", ctx.value()));
    if ignore_ws {
        a.push("--ignore-all-space".into());
    }
    match base {
        ReviewBase::Head => a.push("HEAD".into()),
        ReviewBase::Staged => a.push("--cached".into()),
        ReviewBase::Unstaged => {}
        ReviewBase::Rev(r) => a.push(r.clone()),
    }
    a
}

/// `git add` の引数表。`--` でパスとオプションを必ず切り離す。
pub fn stage_args(path: &str) -> Vec<String> {
    vec!["add".into(), "--".into(), path.to_string()]
}

/// `git restore --staged` の引数表 (index から下ろす)。
pub fn unstage_args(path: &str) -> Vec<String> {
    vec![
        "restore".into(),
        "--staged".into(),
        "--".into(),
        path.to_string(),
    ]
}

/// 「変更を破棄」の引数表。未追跡はファイルごと消すしかないので
/// `git clean -f`、追跡済みは作業ツリーだけ巻き戻す `git restore --worktree`。
/// **どちらも取り消せない**ので UI 側は 2 段確認を必須にすること。
pub fn discard_args(path: &str, untracked: bool) -> Vec<String> {
    if untracked {
        vec!["clean".into(), "-f".into(), "--".into(), path.to_string()]
    } else {
        vec![
            "restore".into(),
            "--worktree".into(),
            "--".into(),
            path.to_string(),
        ]
    }
}

/// `git apply` の引数表 (ハンク単位のステージ / アンステージ / 破棄)。
///
/// パッチは**標準入力**から渡す (`-`)。一時ファイルを作らないので
/// パスを直書きする必要がない = どの OS でもそのまま動く。
///
/// - ステージ:     `cached=true,  reverse=false`
/// - アンステージ: `cached=true,  reverse=true`
/// - 破棄:         `cached=false, reverse=true` (作業ツリーだけ巻き戻す)
pub fn apply_patch_args(cached: bool, reverse: bool) -> Vec<String> {
    let mut a: Vec<String> = vec!["apply".into()];
    if cached {
        a.push("--cached".into());
    }
    if reverse {
        a.push("--reverse".into());
    }
    // 空白の警告だけで失敗させない (本文はこちらが組んだものなので触らせない)
    a.push("--whitespace=nowarn".into());
    a.push("-".into());
    a
}

/// このベースでハンク単位に撃てる操作。
///
/// - 未ステージ (作業ツリー vs index) の差分は `--cached` でそのまま index へ載る
/// - ステージ済み (index vs HEAD) の差分は `--cached --reverse` で index から下りる
/// - 「作業ツリー vs HEAD」や任意リビジョンは**両方の差分が混ざる**ので、
///   パッチが index と一致する保証が無い → ボタンを出さない
/// - `--ignore-all-space` を掛けた差分は原理的に適用できない → 出さない
pub fn hunk_ops_for(
    base: &ReviewBase,
    ignore_ws: bool,
    untracked: bool,
) -> Vec<crate::diff::HunkOp> {
    use crate::diff::HunkOp;
    if ignore_ws || untracked {
        return Vec::new();
    }
    match base {
        ReviewBase::Unstaged => vec![HunkOp::Stage, HunkOp::Discard],
        ReviewBase::Staged => vec![HunkOp::Unstage],
        ReviewBase::Head | ReviewBase::Rev(_) => Vec::new(),
    }
}

/// diff の 1 ファイル + git からしか分からない付随状態。
#[derive(Clone, Debug)]
pub struct ReviewFile {
    pub diff: crate::diff::FileDiff,
    /// ツリーと同じ色分けに使う実効ステータス。
    pub status: crate::git::FileStatus,
    /// index に載っているか (アンステージ可能か)。
    pub staged: bool,
    pub untracked: bool,
    /// git に渡す repo 相対パス (リネームなら新パス)。
    pub path: String,
}

/// 1 回の収集結果。UI スレッドはこれを読むだけ。
#[derive(Clone, Debug, Default)]
pub struct ReviewData {
    pub toplevel: PathBuf,
    pub files: Vec<ReviewFile>,
    /// 上限に当たって diff を途中で切ったか。
    pub truncated: bool,
    /// git が失敗した理由 (リビジョン名の打ち間違いなど)。
    pub error: Option<String>,
}

/// 見出しの「N ファイル変更 · +X −Y」。
pub fn review_summary(files: &[ReviewFile]) -> (usize, usize, usize) {
    let adds = files.iter().map(|f| f.diff.additions).sum();
    let dels = files.iter().map(|f| f.diff.deletions).sum();
    (files.len(), adds, dels)
}

/// diff から実効ステータスを決める (ツリーの色と意味を揃えるため)。
pub fn review_file_status(f: &crate::diff::FileDiff, untracked: bool) -> crate::git::FileStatus {
    use crate::git::FileStatus;
    if untracked {
        return FileStatus::Untracked;
    }
    if f.is_rename {
        return FileStatus::Renamed;
    }
    if f.old_path.is_empty() || f.old_path == "/dev/null" {
        return FileStatus::Added;
    }
    if f.new_path.is_empty() || f.new_path == "/dev/null" {
        return FileStatus::Deleted;
    }
    FileStatus::Modified
}

/// git に渡せる実パス (`display_path` はリネームを "旧 → 新" と書くので使えない)。
pub fn review_path(f: &crate::diff::FileDiff) -> String {
    if !f.new_path.is_empty() && f.new_path != "/dev/null" {
        f.new_path.clone()
    } else {
        f.old_path.clone()
    }
}

/// ファイル一覧をディレクトリごとにまとめる。
/// 戻りは (ディレクトリ, 元の添字) をディレクトリ名順・パス順に並べたもの。
/// "" はリポジトリ直下。
pub fn group_by_dir(paths: &[String]) -> Vec<(String, Vec<usize>)> {
    let mut map: std::collections::BTreeMap<String, Vec<usize>> = std::collections::BTreeMap::new();
    for (i, p) in paths.iter().enumerate() {
        let dir = p.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        map.entry(dir.to_string()).or_default().push(i);
    }
    for v in map.values_mut() {
        v.sort_by(|a, b| paths[*a].cmp(&paths[*b]));
    }
    map.into_iter().collect()
}

/// 次/前の変更へ。端では止まる (回り込まない)。
pub fn move_selection(cur: Option<usize>, len: usize, delta: i32) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let last = len - 1;
    Some(match cur {
        None => {
            if delta >= 0 {
                0
            } else {
                last
            }
        }
        Some(i) => {
            if delta >= 0 {
                (i + delta as usize).min(last)
            } else {
                i.saturating_sub(delta.unsigned_abs() as usize)
            }
        }
    })
}

/// NUL を含むならバイナリ扱い (git 自身と同じ経験則)。
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|b| *b == 0)
}

/// 未追跡ファイルを「全行追加」の unified diff に仕立てる。
///
/// git は untracked を `diff` に出さないので、レビュー画面では合成する。
/// バイナリは中身を出さず `Binary files ...` 行だけにして、
/// diff レンダラ側に「バイナリファイル」と描かせる。
/// `max_bytes` を超えるものは行境界で切り、末尾に省略行を足す
/// (ハンクの宣言行数と実際の行数は必ず一致させる)。
fn synth_untracked_diff(path: &str, bytes: &[u8], max_bytes: usize) -> String {
    let head = format!("diff --git a/{path} b/{path}\nnew file mode 100644\n");
    if looks_binary(bytes) {
        return format!("{head}Binary files /dev/null and b/{path} differ\n");
    }
    let truncated = bytes.len() > max_bytes;
    let capped = &bytes[..bytes.len().min(max_bytes)];
    let text = String::from_utf8_lossy(capped);
    // CRLF の \r は diff 本文に残さない (行末が化けて見えるため)。
    let mut lines: Vec<String> = text
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect();
    if truncated {
        lines.pop(); // 途中で切れた最終行は捨てる
        lines.push("… (大きいため以降を省略)".to_string());
    }
    if lines.is_empty() {
        // 空ファイル: ハンク無しでも「追加された」ことは伝わる
        return format!("{head}--- /dev/null\n+++ b/{path}\n");
    }
    let mut s = format!(
        "{head}--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n",
        lines.len()
    );
    for l in &lines {
        s.push('+');
        s.push_str(l);
        s.push('\n');
    }
    s
}

/// diff テキストを行境界で上限まで切る。戻りは (本文, 切ったか)。
fn cap_diff_text(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut out = String::with_capacity(max_bytes + 64);
    for line in text.lines() {
        if out.len() + line.len() + 1 > max_bytes {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    (out, true)
}

/// NUL 区切りの `--name-only -z` / `ls-files -z` 出力をパスの集合にする。
fn parse_nul_paths(out: &str) -> Vec<String> {
    out.split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// レビュー用データの収集。**バックグラウンドスレッドから呼ぶこと**
/// (git を数回起動し、未追跡ファイルを読む)。
fn collect_review(ws: &Path, base: &ReviewBase, ctx: ContextLines, ignore_ws: bool) -> ReviewData {
    let toplevel = run_git(ws, &["rev-parse", "--show-toplevel"])
        .map(|s| PathBuf::from(s.trim()))
        .unwrap_or_else(|_| ws.to_path_buf());

    let args = review_diff_args(base, ctx, ignore_ws);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut text = match run_git(ws, &argv) {
        Ok(s) => s,
        Err(e) => {
            return ReviewData {
                toplevel,
                error: Some(e.text().to_string()),
                ..Default::default()
            }
        }
    };

    // index に載っているパス (アンステージ可否の判定に使う)
    let staged: Vec<String> = run_git(ws, &["diff", "--cached", "--name-only", "-z"])
        .map(|s| parse_nul_paths(&s))
        .unwrap_or_default();

    // 未追跡は git diff に出ないので合成して足す
    let mut untracked: Vec<String> = Vec::new();
    if base.includes_untracked() {
        untracked = run_git(ws, &["ls-files", "--others", "--exclude-standard", "-z"])
            .map(|s| parse_nul_paths(&s))
            .unwrap_or_default();
        for p in &untracked {
            if text.len() > MAX_REVIEW_DIFF_BYTES {
                break;
            }
            let bytes = std::fs::read(toplevel.join(p)).unwrap_or_default();
            text.push_str(&synth_untracked_diff(p, &bytes, MAX_UNTRACKED_BYTES));
        }
    }

    let (text, truncated) = cap_diff_text(&text, MAX_REVIEW_DIFF_BYTES);
    let mut files: Vec<ReviewFile> = crate::diff::parse_unified(&text)
        .into_iter()
        .map(|d| {
            let path = review_path(&d);
            let untracked = untracked.iter().any(|u| *u == path);
            ReviewFile {
                status: review_file_status(&d, untracked),
                staged: staged.iter().any(|s| *s == path),
                untracked,
                path,
                diff: d,
            }
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));

    ReviewData {
        toplevel,
        files,
        truncated,
        error: None,
    }
}

/// レビュー画面の変更系ジョブ。
enum ReviewJob {
    Stage(String),
    Unstage(String),
    Discard {
        path: String,
        untracked: bool,
    },
    /// ハンク単位。`patch` を標準入力から `git apply` へ流す。
    ApplyHunk {
        label: String,
        patch: String,
        cached: bool,
        reverse: bool,
    },
}

/// PR 風のローカル変更レビュー画面。
///
/// 左 = 変更ファイル一覧 (ディレクトリごと・ツリーと同じ色とバッジ・`+N −M`)、
/// 右 = 既存の diff レンダラ (`diff::diff_ui_with_actions`) による差分。
/// diff の描画をレンダラに任せているので、インラインレビューコメント
/// (行クリック → コメント → まとめてエージェントへ) も、並列 (左右 2 列) 表示・
/// 語単位ハイライト・F7 の変更ジャンプも、この画面でそのまま効く。
/// 表示モードの切替ボタンは 1 フレームに 1 個だけ出る (`diff::claim_mode_toggle`)。
pub struct ReviewPanel {
    workspace: PathBuf,
    base: ReviewBase,
    rev_input: String,
    ctx_lines: ContextLines,
    ignore_ws: bool,
    data: ReviewData,
    loaded: bool,
    last_refresh: Option<Instant>,
    pending: Option<Receiver<ReviewData>>,
    job: Option<Receiver<(String, bool)>>,
    job_label: String,
    selected: Option<usize>,
    /// 次のフレームでこのファイルの diff までスクロールする。
    scroll_to: Option<usize>,
    /// 畳んであるファイル (パス)。巨大 diff を描かないための実質的な間引きでもある。
    collapsed: std::collections::HashSet<String>,
    /// 「変更を破棄」の 2 段確認 (race パネルの [破棄] と同じ流儀)。
    confirm_discard: Option<String>,
    /// ハンク破棄の 2 段確認 `(ファイル添字, ハンク添字)`。**取り消せない**ので必須。
    confirm_hunk: Option<(usize, usize)>,
    /// ファイルごとのインラインコメント。パスを鍵にするので、
    /// 再収集して添字がずれてもコメントは付いたまま。
    comments: std::collections::HashMap<String, crate::diff::DiffCommentStore>,
    /// 一覧をクリックしたか (キーボード操作を横取りしないための足枷)。
    list_focused: bool,
}

impl ReviewPanel {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            base: ReviewBase::default(),
            rev_input: String::new(),
            ctx_lines: ContextLines::default(),
            ignore_ws: false,
            data: ReviewData::default(),
            loaded: false,
            last_refresh: None,
            pending: None,
            job: None,
            job_label: String::new(),
            selected: None,
            scroll_to: None,
            collapsed: std::collections::HashSet::new(),
            confirm_discard: None,
            confirm_hunk: None,
            comments: std::collections::HashMap::new(),
            list_focused: false,
        }
    }

    pub fn set_workspace(&mut self, ws: PathBuf) {
        if self.workspace != ws {
            self.workspace = ws;
            // 旧ワークスペース向けの飛行中の収集は受信口ごと捨てる
            // (旧 repo の結果でステージ/破棄を撃たせない)。
            self.pending = None;
            self.data = ReviewData::default();
            self.loaded = false;
            self.selected = None;
            self.confirm_discard = None;
            self.confirm_hunk = None;
            self.comments.clear();
            self.invalidate();
        }
    }

    /// 次フレームで収集し直す。
    pub fn invalidate(&mut self) {
        self.last_refresh = None;
    }

    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// 比較ベースを外から指定する (コマンドパレット等の配線用)。
    pub fn set_base(&mut self, base: ReviewBase) {
        if self.base != base {
            self.base = base;
            self.selected = None;
            // ベースが変わればハンクの添字も意味も変わる。確認は必ず外す。
            self.confirm_hunk = None;
            self.invalidate();
        }
    }

    /// 毎フレーム呼ぶ。git は一切ここでは起動しない (キャッシュを読むだけ)。
    pub fn ui(&mut self, ui: &mut egui::Ui, theme: &Theme, actions: &mut GitActions) {
        let ctx = ui.ctx().clone();
        self.poll(actions);
        self.maybe_refresh(&ctx);

        self.toolbar_ui(ui, theme);
        self.summary_ui(ui, theme);

        if let Some(err) = self.data.error.clone() {
            ui.label(RichText::new(err).color(theme.err).small());
            return;
        }
        if !self.loaded {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                ui.label(
                    RichText::new(tr("差分を集めています…"))
                        .color(theme.text_dim)
                        .small(),
                );
            });
            return;
        }
        if self.data.files.is_empty() {
            ui.label(
                RichText::new(tr("このベースとの差分はありません"))
                    .color(theme.text_dim)
                    .small(),
            );
            return;
        }
        if self.data.truncated {
            ui.label(
                RichText::new(tr("差分が大きいため一部のみ表示"))
                    .color(theme.warn)
                    .small(),
            )
            .on_hover_text(tr(
                "上限を超えた分は読み込んでいません。文脈行を減らすか、ベースを絞ってください",
            ));
        }

        self.handle_keys(ui);

        let mut job: Option<ReviewJob> = None;
        // 幅が狭いときは縦積み (左サイドバーに置かれても潰れない)。
        let wide = ui.available_width() >= 640.0;
        if wide {
            let list_w = (ui.available_width() * 0.32).clamp(200.0, 380.0);
            let h = ui.available_height();
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(list_w, h),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("zv-review-list")
                            .show(ui, |ui| self.file_list_ui(ui, theme, &mut job, actions));
                    },
                );
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("zv-review-diff")
                    .show(ui, |ui| self.diff_pane_ui(ui, theme, actions, &mut job));
            });
        } else {
            egui::ScrollArea::vertical()
                .id_salt("zv-review-stacked")
                .show(ui, |ui| {
                    self.file_list_ui(ui, theme, &mut job, actions);
                    ui.separator();
                    self.diff_pane_ui(ui, theme, actions, &mut job);
                });
        }

        if let Some(j) = job {
            self.spawn_review_job(&ctx, j, actions);
        }
    }

    // -- 各パーツ -----------------------------------------------------------

    fn toolbar_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(tr("変更レビュー")).strong().color(theme.text));
            if self.pending.is_some() {
                ui.add(egui::Spinner::new().size(11.0));
            }
            ui.label(RichText::new(tr("比較:")).color(theme.text_dim).small());
            let mut next = self.base.clone();
            egui::ComboBox::from_id_salt("zv-review-base")
                .selected_text(self.base.label())
                .show_ui(ui, |ui| {
                    for b in [ReviewBase::Head, ReviewBase::Staged, ReviewBase::Unstaged] {
                        let label = b.label();
                        ui.selectable_value(&mut next, b, label);
                    }
                    let rev = ReviewBase::Rev(self.rev_input.clone());
                    ui.selectable_value(&mut next, rev, tr("ブランチ / コミットを指定…"));
                });
            if next != self.base {
                self.base = next;
                self.selected = None;
                changed = true;
            }
            if matches!(self.base, ReviewBase::Rev(_)) {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.rev_input)
                        .desired_width(150.0)
                        .hint_text("main / origin/main / HEAD~3"),
                );
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    match validate_rev(&self.rev_input) {
                        Ok(rev) => {
                            self.base = ReviewBase::Rev(rev);
                            changed = true;
                        }
                        Err(e) => {
                            ui.label(RichText::new(e).color(theme.err).small());
                        }
                    }
                }
            }
            ui.separator();
            ui.label(RichText::new(tr("文脈:")).color(theme.text_dim).small());
            for c in [ContextLines::Three, ContextLines::Ten, ContextLines::All] {
                if ui
                    .selectable_label(self.ctx_lines == c, tr(c.label()))
                    .clicked()
                    && self.ctx_lines != c
                {
                    self.ctx_lines = c;
                    changed = true;
                }
            }
            if ui
                .selectable_label(self.ignore_ws, tr("空白無視"))
                .on_hover_text(tr(
                    "インデントだけの変更を差分から外す (--ignore-all-space)",
                ))
                .clicked()
            {
                self.ignore_ws = !self.ignore_ws;
                changed = true;
            }
            if ui.button(tr("再読み込み")).clicked() {
                changed = true;
            }
        });
        if changed {
            self.invalidate();
        }
    }

    fn summary_ui(&self, ui: &mut egui::Ui, theme: &Theme) {
        let (n, adds, dels) = review_summary(&self.data.files);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(trf("{n} ファイル変更", &[("n", n.to_string())]))
                    .color(theme.text)
                    .strong(),
            );
            ui.label(RichText::new("·").color(theme.text_dim));
            ui.label(
                RichText::new(format!("+{adds}"))
                    .color(theme.ok)
                    .monospace(),
            );
            ui.label(
                RichText::new(format!("−{dels}"))
                    .color(theme.err)
                    .monospace(),
            );
        });
    }

    fn file_list_ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        job: &mut Option<ReviewJob>,
        actions: &mut GitActions,
    ) {
        let paths: Vec<String> = self.data.files.iter().map(|f| f.path.clone()).collect();
        for (dir, idxs) in group_by_dir(&paths) {
            let title = if dir.is_empty() {
                tr("(リポジトリ直下)")
            } else {
                dir.clone()
            };
            ui.label(RichText::new(title).color(theme.text_dim).small());
            for i in idxs {
                let (status, path, adds, dels, staged, untracked) = {
                    let f = &self.data.files[i];
                    (
                        f.status,
                        f.path.clone(),
                        f.diff.additions,
                        f.diff.deletions,
                        f.staged,
                        f.untracked,
                    )
                };
                let (color, badge, hint) = crate::file_tree::git_status_style(status, theme);
                let name = path.rsplit_once('/').map(|(_, n)| n).unwrap_or(&path);
                ui.horizontal(|ui| {
                    let sel = self.selected == Some(i);
                    let label = format!("{badge}  {name}");
                    let resp = ui
                        .selectable_label(sel, RichText::new(label).color(color))
                        .on_hover_text(format!("{path}\n{}", tr(hint)));
                    if resp.clicked() {
                        self.selected = Some(i);
                        self.scroll_to = Some(i);
                        self.list_focused = true;
                        self.confirm_discard = None;
                    }
                    ui.label(RichText::new(format!("+{adds}")).color(theme.ok).small());
                    ui.label(RichText::new(format!("−{dels}")).color(theme.err).small());
                    if staged {
                        ui.label(RichText::new(tr("済")).color(theme.accent).small())
                            .on_hover_text(tr("ステージ済み"));
                    }
                });
                if self.selected == Some(i) {
                    self.file_actions_ui(ui, theme, &path, staged, untracked, job, actions);
                }
            }
        }
    }

    /// 選択中ファイルの操作行。破棄だけ 2 段確認。
    #[allow(clippy::too_many_arguments)]
    fn file_actions_ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        path: &str,
        staged: bool,
        untracked: bool,
        job: &mut Option<ReviewJob>,
        actions: &mut GitActions,
    ) {
        let busy = self.job.is_some();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!busy, egui::Button::new(tr("ステージ")).small())
                .clicked()
            {
                *job = Some(ReviewJob::Stage(path.to_string()));
            }
            if ui
                .add_enabled(
                    !busy && staged,
                    egui::Button::new(tr("アンステージ")).small(),
                )
                .clicked()
            {
                *job = Some(ReviewJob::Unstage(path.to_string()));
            }
            // [変更を破棄] — 1 度目は警告に変わるだけ (race パネルと同じ 2 段)
            if self.confirm_discard.as_deref() == Some(path) {
                if ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(RichText::new(tr("⚠ 本当に破棄")).color(theme.err))
                            .small(),
                    )
                    .on_hover_text(tr("この操作は取り消せません"))
                    .clicked()
                {
                    *job = Some(ReviewJob::Discard {
                        path: path.to_string(),
                        untracked,
                    });
                    self.confirm_discard = None;
                }
            } else if ui
                .add_enabled(!busy, egui::Button::new(tr("変更を破棄")).small())
                .on_hover_text(tr("もう一度押すと確定します (取り消せません)"))
                .clicked()
            {
                self.confirm_discard = Some(path.to_string());
            }
            if ui
                .button(RichText::new(tr("エディタで開く")).small())
                .clicked()
            {
                actions.open_file = Some(self.data.toplevel.join(path));
            }
        });
    }

    fn diff_pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        actions: &mut GitActions,
        job: &mut Option<ReviewJob>,
    ) {
        let scroll_to = self.scroll_to.take();
        let mut prompt: Option<String> = None;
        let mut hunk_hit: Option<(usize, usize, crate::diff::HunkOp)> = None;
        for i in 0..self.data.files.len() {
            let path = self.data.files[i].path.clone();
            let collapsed = self.collapsed.contains(&path);
            let block = ui.scope(|ui| {
                ui.horizontal(|ui| {
                    let arrow = if collapsed { "▶" } else { "▼" };
                    if ui.small_button(arrow).clicked() {
                        if collapsed {
                            self.collapsed.remove(&path);
                        } else {
                            self.collapsed.insert(path.clone());
                        }
                    }
                    let (color, badge, _) =
                        crate::file_tree::git_status_style(self.data.files[i].status, theme);
                    ui.label(RichText::new(badge).color(color).monospace());
                    if self.selected == Some(i) {
                        ui.label(RichText::new("◀").color(theme.accent).small());
                    }
                    // 畳んでいる間は diff レンダラの見出しが出ないので、
                    // パスと増減はこちらで出す (何を畳んだか分かるように)。
                    if collapsed {
                        let f = &self.data.files[i];
                        ui.label(RichText::new(&f.path).color(theme.text).monospace());
                        ui.label(
                            RichText::new(format!("+{}", f.diff.additions))
                                .color(theme.ok)
                                .small(),
                        );
                        ui.label(
                            RichText::new(format!("−{}", f.diff.deletions))
                                .color(theme.err)
                                .small(),
                        );
                    }
                });
                if !collapsed {
                    // 既存の diff レンダラをそのまま使う。構文色も、
                    // 行クリックのインラインレビューコメントもこれで効く。
                    let ops = hunk_ops_for(
                        &self.base,
                        self.ignore_ws,
                        self.data.files[i].untracked || self.data.files[i].diff.is_binary,
                    );
                    let confirm = self.confirm_hunk.filter(|(f, _)| *f == i).map(|(_, h)| h);
                    let store = self.comments.entry(path.clone()).or_default();
                    let action = crate::diff::diff_ui_with_hunk_actions(
                        ui,
                        theme,
                        std::slice::from_ref(&self.data.files[i].diff),
                        store,
                        Some(crate::diff::HunkActions {
                            ops: &ops,
                            confirm_discard: confirm,
                        }),
                    );
                    match action {
                        crate::diff::DiffAction::SendToAgent(p) => prompt = Some(p),
                        crate::diff::DiffAction::Hunk { hunk, op, .. } => {
                            hunk_hit = Some((i, hunk, op));
                        }
                        crate::diff::DiffAction::None => {}
                    }
                }
            });
            if scroll_to == Some(i) {
                ui.scroll_to_rect(block.response.rect, Some(egui::Align::TOP));
            }
            ui.add_space(4.0);
        }
        if let Some(p) = prompt {
            actions.review_prompt = Some(p);
        }
        if let Some((fi, hi, op)) = hunk_hit {
            self.on_hunk_op(fi, hi, op, job, actions);
        }
    }

    /// ハンクのボタンが押されたときの分岐。
    /// **破棄だけは 2 段確認**を挟む (取り消せないので)。
    fn on_hunk_op(
        &mut self,
        fi: usize,
        hi: usize,
        op: crate::diff::HunkOp,
        job: &mut Option<ReviewJob>,
        actions: &mut GitActions,
    ) {
        use crate::diff::HunkOp;
        if op == HunkOp::Discard && self.confirm_hunk != Some((fi, hi)) {
            self.confirm_hunk = Some((fi, hi));
            return;
        }
        self.confirm_hunk = None;
        let Some(f) = self.data.files.get(fi) else {
            return;
        };
        let Some(patch) = crate::diff::build_hunk_patch(&f.diff, hi) else {
            actions.toast = Some((tr("このハンクはパッチにできません"), false));
            return;
        };
        let (label, cached, reverse) = match op {
            HunkOp::Stage => (
                trf("ハンクをステージ {p}", &[("p", f.path.clone())]),
                true,
                false,
            ),
            HunkOp::Unstage => (
                trf("ハンクをアンステージ {p}", &[("p", f.path.clone())]),
                true,
                true,
            ),
            HunkOp::Discard => (
                trf("ハンクを破棄 {p}", &[("p", f.path.clone())]),
                false,
                true,
            ),
        };
        *job = Some(ReviewJob::ApplyHunk {
            label,
            patch,
            cached,
            reverse,
        });
    }

    /// n / p / ↓ / ↑ で次・前の変更へ。
    ///
    /// 横取り防止のため 3 つ揃ったときだけ効かせる:
    /// 一覧を一度クリックしている / ポインタがこのパネル上にある /
    /// テキスト入力にフォーカスが無い。エディタで打鍵中に奪わない。
    fn handle_keys(&mut self, ui: &mut egui::Ui) {
        if !self.list_focused || !ui.ui_contains_pointer() || ui.memory(|m| m.focused().is_some()) {
            return;
        }
        let len = self.data.files.len();
        let delta = ui.input(|i| {
            let next = i.key_pressed(egui::Key::N) || i.key_pressed(egui::Key::ArrowDown);
            let prev = i.key_pressed(egui::Key::P) || i.key_pressed(egui::Key::ArrowUp);
            match (next, prev) {
                (true, false) => 1,
                (false, true) => -1,
                _ => 0,
            }
        });
        if delta != 0 {
            let next = move_selection(self.selected, len, delta);
            if next != self.selected {
                self.selected = next;
                self.scroll_to = next;
                self.confirm_discard = None;
            }
        }
    }

    // -- ジョブ管理 ---------------------------------------------------------

    fn poll(&mut self, actions: &mut GitActions) {
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(data) => {
                    self.data = data;
                    self.loaded = true;
                    self.pending = None;
                    // 再収集でハンクの並びが変わる。破棄の確認は必ず外す
                    // (別のハンクを消してしまわないように)。
                    self.confirm_hunk = None;
                    // 選択が範囲外になったら外す
                    if self.selected.is_some_and(|i| i >= self.data.files.len()) {
                        self.selected = None;
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.job {
            match rx.try_recv() {
                Ok(msg) => {
                    actions.toast = Some(msg);
                    self.job = None;
                    self.job_label.clear();
                    self.invalidate();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.job = None;
                    self.job_label.clear();
                    self.invalidate();
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    fn maybe_refresh(&mut self, ctx: &egui::Context) {
        if self.pending.is_some() {
            return;
        }
        if let Some(t) = self.last_refresh {
            if t.elapsed() < REVIEW_TTL {
                return;
            }
        }
        self.last_refresh = Some(Instant::now());
        let (tx, rx) = mpsc::channel();
        let ws = self.workspace.clone();
        let base = self.base.clone();
        let cl = self.ctx_lines;
        let iw = self.ignore_ws;
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("zv-git-review".into())
            .spawn(move || {
                let data = collect_review(&ws, &base, cl, iw);
                let _ = tx.send(data);
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => self.pending = Some(rx),
            Err(e) => {
                self.loaded = true;
                self.data.error = Some(trf("差分を取得できません: {e}", &[("e", e.to_string())]));
            }
        }
    }

    fn spawn_review_job(&mut self, ctx: &egui::Context, job: ReviewJob, actions: &mut GitActions) {
        if self.job.is_some() {
            return;
        }
        let (label, args, patch) = match job {
            ReviewJob::Stage(p) => (
                trf("ステージ {p}", &[("p", p.clone())]),
                stage_args(&p),
                None,
            ),
            ReviewJob::Unstage(p) => (
                trf("アンステージ {p}", &[("p", p.clone())]),
                unstage_args(&p),
                None,
            ),
            ReviewJob::Discard { path, untracked } => (
                trf("破棄 {p}", &[("p", path.clone())]),
                discard_args(&path, untracked),
                None,
            ),
            ReviewJob::ApplyHunk {
                label,
                patch,
                cached,
                reverse,
            } => (label, apply_patch_args(cached, reverse), Some(patch)),
        };
        let (tx, rx) = mpsc::channel();
        let ws = self.workspace.clone();
        let ctx2 = ctx.clone();
        let label2 = label.clone();
        let spawned = std::thread::Builder::new()
            .name("zv-git-review-job".into())
            .spawn(move || {
                let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                let run = match &patch {
                    Some(p) => run_git_stdin(&ws, &argv, p),
                    None => run_git(&ws, &argv),
                };
                let msg = match run {
                    Ok(_) => (trf("{label2} 完了", &[("label2", label2.clone())]), true),
                    Err(e) => (
                        trf(
                            "{label2} 失敗: {e}",
                            &[("label2", label2.clone()), ("e", e.text().to_string())],
                        ),
                        false,
                    ),
                };
                let _ = tx.send(msg);
                ctx2.request_repaint();
            });
        match spawned {
            Ok(_) => {
                self.job = Some(rx);
                self.job_label = label;
            }
            Err(e) => {
                actions.toast = Some((
                    trf("git を起動できません: {e}", &[("e", e.to_string())]),
                    false,
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// テスト (git を起動しない純粋なパース / 検証のみ)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const WORKTREE_FIXTURE: &str = "\
worktree /Users/me/dev/zaivern-code
HEAD 2f14c3e9a1b2c3d4e5f60718293a4b5c6d7e8f90
branch refs/heads/main

worktree /Users/me/dev/zaivern-code/.claude/worktrees/voice-cross-platform
HEAD aabbccdd11223344556677889900aabbccddeeff
branch refs/heads/voice-cross-platform

worktree /Users/me/dev/detached-wt
HEAD 1234567890abcdef1234567890abcdef12345678
detached
locked claude session wt (pid 14833 start Mon Jul 20 15:49:26 2026)

worktree /Users/me/dev/bare-repo.git
bare

";

    #[test]
    fn worktree_porcelain_parses_records() {
        let v = parse_worktree_porcelain(WORKTREE_FIXTURE);
        assert_eq!(v.len(), 4);
        assert_eq!(v[0].path, PathBuf::from("/Users/me/dev/zaivern-code"));
        assert_eq!(v[0].branch.as_deref(), Some("main"));
        assert!(!v[0].detached && !v[0].bare);
        assert_eq!(v[1].branch.as_deref(), Some("voice-cross-platform"));
    }

    #[test]
    fn worktree_porcelain_handles_detached_and_bare() {
        let v = parse_worktree_porcelain(WORKTREE_FIXTURE);
        assert!(v[2].detached);
        assert!(v[2].locked);
        assert_eq!(v[2].branch, None);
        assert!(v[2].label().starts_with("(detached 12345678"));

        assert!(v[3].bare);
        assert_eq!(v[3].head, None);
        assert_eq!(v[3].label(), "(bare)");
    }

    #[test]
    fn worktree_porcelain_without_trailing_blank_line() {
        let out = "worktree /a\nHEAD ff\nbranch refs/heads/x";
        let v = parse_worktree_porcelain(out);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].branch.as_deref(), Some("x"));
    }

    #[test]
    fn worktree_porcelain_empty_is_empty() {
        assert!(parse_worktree_porcelain("").is_empty());
        assert!(parse_worktree_porcelain("\n\n\n").is_empty());
    }

    #[test]
    fn branch_list_marks_current_and_remotes() {
        // 行頭 2 文字のマーカーを潰さないよう concat! で組む
        let out = concat!(
            "  feature/login\n",
            "* main\n",
            "+ voice-cross-platform\n",
            "  remotes/origin/HEAD -> origin/main\n",
            "  remotes/origin/main\n",
            "  remotes/origin/feature/login\n",
        );
        let b = parse_branch_list(out);
        assert_eq!(b.local.len(), 3);
        assert_eq!(b.head, Some(HeadState::OnBranch("main".into())));

        let main = b.local.iter().find(|x| x.name == "main").unwrap();
        assert!(main.current && !main.other_worktree);

        let wt = b
            .local
            .iter()
            .find(|x| x.name == "voice-cross-platform")
            .unwrap();
        assert!(!wt.current && wt.other_worktree);

        // "origin/HEAD -> origin/main" は捨てる
        assert_eq!(b.remote, vec!["origin/main", "origin/feature/login"]);
    }

    #[test]
    fn branch_list_detects_detached_head() {
        let out = concat!("* (HEAD detached at 2f14c3e)\n", "  main\n", "  develop\n",);
        let b = parse_branch_list(out);
        assert_eq!(
            b.head,
            Some(HeadState::Detached("HEAD detached at 2f14c3e".into()))
        );
        // detached 行はブランチ一覧に混ぜない
        assert_eq!(b.local.len(), 2);
        assert!(b.local.iter().all(|x| !x.current));
    }

    #[test]
    fn branch_list_handles_no_branch_form() {
        let b = parse_branch_list("* (no branch)\n  main\n");
        assert_eq!(b.head, Some(HeadState::Detached("no branch".into())));
        assert_eq!(b.local.len(), 1);
    }

    #[test]
    fn branch_list_empty_repo() {
        let b = parse_branch_list("");
        assert!(b.local.is_empty() && b.remote.is_empty());
        assert_eq!(b.head, None);
    }

    #[test]
    fn status_porcelain_parses_codes_and_paths() {
        // XY の 2 文字は先頭の空白まで意味を持つので concat! で厳密に組む
        let out = concat!(
            " M src/app.rs\n",
            "M  src/git.rs\n",
            "A  src/git_panel.rs\n",
            " D src/gone.rs\n",
            "?? scratch/notes.txt\n",
            "UU src/conflict.rs\n",
        );
        let v = parse_status_porcelain(out);
        assert_eq!(v.len(), 6);
        assert_eq!(v[0].code, " M");
        assert_eq!(v[0].path, "src/app.rs");
        assert_eq!(v[0].letter(), 'M');
        assert_eq!(v[1].letter(), 'M');
        assert_eq!(v[2].letter(), 'A');
        assert_eq!(v[3].letter(), 'D');
        assert_eq!(v[5].letter(), 'U');
    }

    #[test]
    fn status_porcelain_parses_renames_and_untracked() {
        let out = "R  src/old_name.rs -> src/new_name.rs\n?? tmp/out.log\nC  a.rs -> b.rs\n";
        let v = parse_status_porcelain(out);
        assert_eq!(v.len(), 3);

        assert_eq!(v[0].path, "src/new_name.rs");
        assert_eq!(v[0].orig.as_deref(), Some("src/old_name.rs"));
        assert_eq!(v[0].letter(), 'R');

        assert_eq!(v[1].path, "tmp/out.log");
        assert_eq!(v[1].orig, None);
        assert!(v[1].untracked());
        assert_eq!(v[1].letter(), '?');

        assert_eq!(v[2].orig.as_deref(), Some("a.rs"));
        assert_eq!(v[2].path, "b.rs");
    }

    #[test]
    fn status_porcelain_ignores_junk_and_multibyte_is_safe() {
        assert!(parse_status_porcelain("").is_empty());
        assert!(parse_status_porcelain("x\n").is_empty());
        // 日本語ファイル名でも境界チェックで落ちない
        let v = parse_status_porcelain(" M ドキュメント/メモ.md\n");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, "ドキュメント/メモ.md");
        // 行頭からマルチバイト文字の行 (is_char_boundary(2) が偽) でも落ちない
        assert!(parse_status_porcelain("あいう\n").is_empty());
        assert!(parse_status_porcelain("アM path\n").is_empty());
    }

    #[test]
    fn validate_branch_name_rejects_empty_and_dash() {
        assert!(validate_branch_name("").is_err());
        assert!(validate_branch_name("   ").is_err());
        assert!(validate_branch_name("\t\n").is_err());
        assert!(validate_branch_name("-f").is_err());
        assert!(validate_branch_name("--force").is_err());
        assert!(validate_branch_name("  -D  ").is_err());
    }

    #[test]
    fn validate_branch_name_rejects_unsafe_chars() {
        assert!(validate_branch_name("has space").is_err());
        assert!(validate_branch_name("a..b").is_err());
        assert!(validate_branch_name("a~1").is_err());
        assert!(validate_branch_name("a:b").is_err());
        assert!(validate_branch_name("a?b").is_err());
        assert!(validate_branch_name("main@{1}").is_err());
        assert!(validate_branch_name("/leading").is_err());
        assert!(validate_branch_name("trailing/").is_err());
        assert!(validate_branch_name("x.lock").is_err());
    }

    #[test]
    fn validate_branch_name_accepts_normal_names() {
        assert_eq!(validate_branch_name("  main  ").unwrap(), "main");
        assert_eq!(
            validate_branch_name("feature/login-v2").unwrap(),
            "feature/login-v2"
        );
        assert_eq!(
            validate_branch_name("日本語ブランチ").unwrap(),
            "日本語ブランチ"
        );
    }

    #[test]
    fn validate_worktree_input_rejects_empty_and_dash() {
        assert!(validate_worktree_input("").is_err());
        assert!(validate_worktree_input("  ").is_err());
        assert!(validate_worktree_input("-x").is_err());
        assert!(validate_worktree_input("../escape").is_err());
        assert_eq!(validate_worktree_input(" wt1 ").unwrap(), "wt1");
    }

    #[test]
    fn worktree_target_defaults_next_to_repo() {
        let main = Path::new("/Users/me/dev/zaivern-code");
        let (p, b) = resolve_worktree_target(main, " my-feature ").unwrap();
        assert_eq!(
            p,
            PathBuf::from("/Users/me/dev/zaivern-code-worktrees/my-feature")
        );
        assert_eq!(b, "my-feature");
    }

    #[test]
    fn worktree_target_accepts_absolute_path() {
        let main = Path::new("/Users/me/dev/zaivern-code");
        let (p, b) = resolve_worktree_target(main, "/tmp/wt/experiment").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/wt/experiment"));
        assert_eq!(b, "experiment");
    }

    #[test]
    fn worktree_target_rejects_bad_input() {
        let main = Path::new("/Users/me/dev/zaivern-code");
        assert!(resolve_worktree_target(main, "").is_err());
        assert!(resolve_worktree_target(main, "-rf").is_err());
        assert!(resolve_worktree_target(main, "a b").is_err());
    }

    #[test]
    fn default_worktree_base_handles_root() {
        assert_eq!(
            default_worktree_base(Path::new("/repo")),
            PathBuf::from("/repo-worktrees")
        );
    }

    // ── PR 風レビュー: 純関数 (git を起動しない) ─────────────────────

    #[test]
    fn review_diff_args_table() {
        let three = ContextLines::Three;
        assert_eq!(
            review_diff_args(&ReviewBase::Head, three, false),
            vec!["diff", "--no-color", "-M", "--unified=3", "HEAD"]
        );
        assert_eq!(
            review_diff_args(&ReviewBase::Staged, three, false),
            vec!["diff", "--no-color", "-M", "--unified=3", "--cached"]
        );
        assert_eq!(
            review_diff_args(&ReviewBase::Unstaged, three, false),
            vec!["diff", "--no-color", "-M", "--unified=3"]
        );
        assert_eq!(
            review_diff_args(
                &ReviewBase::Rev("origin/main".into()),
                ContextLines::Ten,
                true
            ),
            vec![
                "diff",
                "--no-color",
                "-M",
                "--unified=10",
                "--ignore-all-space",
                "origin/main"
            ]
        );
        // 「全部」は git に十分大きい文脈行数を渡す
        assert!(
            review_diff_args(&ReviewBase::Head, ContextLines::All, false)
                .contains(&"--unified=100000".to_string())
        );
        // 未追跡の合成対象は index 比較のときだけ外れる
        assert!(ReviewBase::Head.includes_untracked());
        assert!(ReviewBase::Unstaged.includes_untracked());
        assert!(!ReviewBase::Staged.includes_untracked());
    }

    #[test]
    fn stage_unstage_discard_arg_tables() {
        assert_eq!(stage_args("src/a.rs"), vec!["add", "--", "src/a.rs"]);
        assert_eq!(
            unstage_args("src/a.rs"),
            vec!["restore", "--staged", "--", "src/a.rs"]
        );
        assert_eq!(
            discard_args("src/a.rs", false),
            vec!["restore", "--worktree", "--", "src/a.rs"]
        );
        assert_eq!(
            discard_args("new.txt", true),
            vec!["clean", "-f", "--", "new.txt"]
        );
        // `--` を必ず挟むので、- 始まりのパスでもオプション扱いされない
        for args in [
            stage_args("-weird.rs"),
            unstage_args("-weird.rs"),
            discard_args("-weird.rs", false),
            discard_args("-weird.rs", true),
        ] {
            let dd = args.iter().position(|a| a == "--").expect("-- が要る");
            assert_eq!(args.last().map(String::as_str), Some("-weird.rs"));
            assert_eq!(dd, args.len() - 2, "-- の直後がパス: {args:?}");
        }
    }

    #[test]
    fn validate_rev_allows_revisions_but_rejects_options() {
        for ok in ["main", "origin/main", "HEAD~3", "a1b2c3d", "v1.0..HEAD"] {
            assert_eq!(validate_rev(ok).as_deref(), Ok(ok), "{ok}");
        }
        assert!(validate_rev("").is_err());
        assert!(validate_rev("   ").is_err());
        assert!(validate_rev("--upload-pack=evil").is_err());
        assert!(validate_rev("a b").is_err());
        assert!(validate_rev("a\nb").is_err());
        // 前後の空白は落として受ける
        assert_eq!(validate_rev("  main  ").as_deref(), Ok("main"));
    }

    #[test]
    fn move_selection_clamps_at_both_ends() {
        assert_eq!(move_selection(None, 0, 1), None, "空なら選べない");
        assert_eq!(move_selection(None, 3, 1), Some(0));
        assert_eq!(move_selection(None, 3, -1), Some(2));
        assert_eq!(move_selection(Some(0), 3, 1), Some(1));
        assert_eq!(move_selection(Some(2), 3, 1), Some(2), "末尾で止まる");
        assert_eq!(move_selection(Some(0), 3, -1), Some(0), "先頭で止まる");
    }

    #[test]
    fn group_by_dir_sorts_and_keeps_root_bucket() {
        let paths: Vec<String> = ["src/b.rs", "a.txt", "src/a.rs", "src/deep/z.rs"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let g = group_by_dir(&paths);
        assert_eq!(g[0].0, "", "ルート直下が先頭");
        assert_eq!(g[0].1, vec![1]);
        assert_eq!(g[1].0, "src");
        assert_eq!(g[1].1, vec![2, 0], "ディレクトリ内はパス順");
        assert_eq!(g[2].0, "src/deep");
    }

    /// 未追跡ファイルの合成 diff: 本文・バイナリ・CRLF・日本語・上限。
    #[test]
    fn synth_untracked_diff_covers_text_binary_crlf_and_japanese() {
        // 通常のテキスト → 全行追加のハンク
        let d = synth_untracked_diff("new.rs", b"one\ntwo\n", MAX_UNTRACKED_BYTES);
        assert!(d.contains("new file mode 100644"), "{d}");
        assert!(d.contains("--- /dev/null"));
        assert!(d.contains("@@ -0,0 +1,2 @@"));
        let f = &crate::diff::parse_unified(&d)[0];
        assert_eq!(f.additions, 2);
        assert_eq!(f.new_path, "new.rs");
        assert!(!f.is_binary);

        // CRLF は \r を落として本文だけにする
        let d = synth_untracked_diff("crlf.txt", b"a\r\nb\r\n", MAX_UNTRACKED_BYTES);
        let f = &crate::diff::parse_unified(&d)[0];
        let bodies: Vec<&str> = f.hunks[0].lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(bodies, vec!["a", "b"], "\\r が残らない");

        // 日本語のパスと中身
        let d = synth_untracked_diff(
            "ドキュメント/説明.md",
            "# 見出し\n本文です\n".as_bytes(),
            MAX_UNTRACKED_BYTES,
        );
        let f = &crate::diff::parse_unified(&d)[0];
        assert_eq!(f.new_path, "ドキュメント/説明.md");
        assert_eq!(f.hunks[0].lines[0].text, "# 見出し");

        // バイナリは中身を出さない
        let d = synth_untracked_diff("logo.png", b"\x89PNG\x00\x01\x02", MAX_UNTRACKED_BYTES);
        let f = &crate::diff::parse_unified(&d)[0];
        assert!(f.is_binary, "バイナリ判定: {d}");
        assert!(f.hunks.is_empty(), "本文は出さない");

        // 空ファイルはハンク無しでも壊れない
        let f = crate::diff::parse_unified(&synth_untracked_diff("empty", b"", 100));
        assert_eq!(f.len(), 1);
        assert!(f[0].hunks.is_empty());

        // 上限超過: 宣言行数と実際の行数が一致したまま省略行が入る
        let big: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let d = synth_untracked_diff("big.txt", big.as_bytes(), 200);
        let f = &crate::diff::parse_unified(&d)[0];
        assert_eq!(
            f.additions,
            f.hunks[0].lines.len(),
            "ハンク宣言と実行数が一致 (パースが壊れない)"
        );
        assert!(
            f.hunks[0].lines.last().expect("行").text.contains("省略"),
            "省略が明示される"
        );
    }

    #[test]
    fn cap_diff_text_cuts_on_line_boundary() {
        let text = "aaaa\nbbbb\ncccc\n";
        let (out, cut) = cap_diff_text(text, 1000);
        assert!(!cut);
        assert_eq!(out, text);

        let (out, cut) = cap_diff_text(text, 6);
        assert!(cut, "上限超過を伝える");
        assert_eq!(out, "aaaa\n", "行の途中では切らない");
    }

    #[test]
    fn review_summary_and_status_mapping() {
        use crate::git::FileStatus;
        let mk = |path: &str, adds: usize, dels: usize, untracked: bool| {
            let mut d = crate::diff::parse_unified(&format!(
                "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,{dels} +1,{adds} @@\n{}{}",
                "-x\n".repeat(dels),
                "+y\n".repeat(adds)
            ))
            .remove(0);
            d.additions = adds;
            d.deletions = dels;
            ReviewFile {
                status: review_file_status(&d, untracked),
                staged: false,
                untracked,
                path: review_path(&d),
                diff: d,
            }
        };
        let files = vec![mk("a.rs", 3, 1, false), mk("b.rs", 10, 4, true)];
        assert_eq!(review_summary(&files), (2, 13, 5));
        assert_eq!(review_summary(&[]), (0, 0, 0));
        assert_eq!(files[0].status, FileStatus::Modified);
        assert_eq!(files[1].status, FileStatus::Untracked, "未追跡が最優先");

        // 追加 / 削除 / リネームの判定
        let added = &crate::diff::parse_unified(
            "diff --git a/n.rs b/n.rs\nnew file mode 100644\n--- /dev/null\n+++ b/n.rs\n@@ -0,0 +1,1 @@\n+x\n",
        )[0];
        assert_eq!(review_file_status(added, false), FileStatus::Added);
        assert_eq!(review_path(added), "n.rs");

        let removed = &crate::diff::parse_unified(
            "diff --git a/g.rs b/g.rs\ndeleted file mode 100644\n--- a/g.rs\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-x\n",
        )[0];
        assert_eq!(review_file_status(removed, false), FileStatus::Deleted);
        assert_eq!(review_path(removed), "g.rs", "消えたファイルは旧パスで扱う");

        let renamed = &crate::diff::parse_unified(
            "diff --git a/old.rs b/new.rs\nsimilarity index 95%\nrename from old.rs\nrename to new.rs\n",
        )[0];
        assert_eq!(review_file_status(renamed, false), FileStatus::Renamed);
        assert_eq!(review_path(renamed), "new.rs", "リネームは新パスで操作する");
    }

    // ── PR 風レビュー: 実 git フィクスチャ ───────────────────────────

    /// `git init` + 初回コミット済みの使い捨て repo。git が無ければ None。
    fn review_repo(tag: &str) -> Option<PathBuf> {
        let dir = crate::test_util::unique_temp_dir("zaivern-review-test", tag);
        std::fs::create_dir_all(&dir).ok()?;
        if !git_ok(&dir, &["init", "--quiet"]) {
            std::fs::remove_dir_all(&dir).ok();
            return None;
        }
        // Windows の既定では core.autocrlf=true のため checkout で LF が CRLF に
        // 書き換わり、「作業ツリーが HEAD に戻る」検証がバイト比較で落ちる。
        // テストの意図は改行変換ではなく git の状態遷移なので固定する。
        git_ok(&dir, &["config", "core.autocrlf", "false"]);
        git_ok(&dir, &["config", "core.eol", "lf"]);
        std::fs::write(dir.join("keep.rs"), "fn main() {}\n").ok()?;
        std::fs::create_dir_all(dir.join("src/deep")).ok()?;
        std::fs::write(dir.join("src/deep/mod.rs"), "// one\n// two\n").ok()?;
        git_ok(&dir, &["add", "-A"]);
        git_commit(&dir, "init");
        Some(dir)
    }

    fn git_ok(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn git_commit(dir: &Path, msg: &str) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=zv", "-c", "user.email=zv@example.com"])
            .args(["commit", "--quiet", "-m", msg])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// ステージ済み / 未ステージ / 未追跡 / バイナリ / リネーム / 日本語 / CRLF が
    /// 1 回の収集で全部そろい、ベースを切り替えると集合が変わること。
    #[test]
    fn collect_review_covers_all_change_kinds_and_bases() {
        let Some(repo) = review_repo("kinds") else {
            return; // git が無い環境ではスキップ
        };
        // リネーム元を先にコミットしておく (後続の編集を巻き込まないため)
        std::fs::write(repo.join("moved-src.txt"), "aaa\nbbb\nccc\n").expect("seed");
        git_ok(&repo, &["add", "-A"]);
        git_commit(&repo, "seed rename src");
        // リネーム (index 上)
        git_ok(&repo, &["mv", "moved-src.txt", "moved-dst.txt"]);
        // 未ステージの変更
        std::fs::write(repo.join("keep.rs"), "fn main() { changed(); }\n").expect("modify");
        // ステージ済みの変更
        std::fs::write(repo.join("src/deep/mod.rs"), "// one\n// two\n// three\n").expect("stage");
        git_ok(&repo, &["add", "--", "src/deep/mod.rs"]);
        // 未追跡: 通常テキスト / 日本語パス / CRLF / バイナリ
        std::fs::write(repo.join("untracked.txt"), "new file\n").expect("untracked");
        std::fs::write(repo.join("日本語ファイル.md"), "# 見出し\n本文\n").expect("ja");
        std::fs::write(repo.join("crlf.txt"), "a\r\nb\r\n").expect("crlf");
        std::fs::write(repo.join("blob.bin"), [0u8, 1, 2, 3, 0, 9]).expect("bin");

        let head = collect_review(&repo, &ReviewBase::Head, ContextLines::Three, false);
        assert!(head.error.is_none(), "{:?}", head.error);
        let names: Vec<&str> = head.files.iter().map(|f| f.path.as_str()).collect();
        for want in [
            "keep.rs",
            "src/deep/mod.rs",
            "untracked.txt",
            "日本語ファイル.md",
            "crlf.txt",
            "blob.bin",
            "moved-dst.txt",
        ] {
            assert!(
                names.contains(&want),
                "HEAD 比較に {want} が出る: {names:?}"
            );
        }
        let by = |p: &str| head.files.iter().find(|f| f.path == p).expect(p);
        assert!(by("blob.bin").diff.is_binary, "バイナリは中身を出さない");
        assert!(by("blob.bin").untracked);
        assert_eq!(by("blob.bin").status, crate::git::FileStatus::Untracked);
        assert!(by("src/deep/mod.rs").staged, "index に載っている");
        assert!(!by("keep.rs").staged, "未ステージ");
        assert_eq!(
            by("日本語ファイル.md").diff.hunks[0].lines[0].text,
            "# 見出し"
        );
        let crlf = by("crlf.txt");
        assert!(
            crlf.diff.hunks[0]
                .lines
                .iter()
                .all(|l| !l.text.contains('\r')),
            "CRLF の \\r を持ち込まない"
        );
        // 合計は各ファイルの +/- の総和
        let (n, adds, dels) = review_summary(&head.files);
        assert_eq!(n, head.files.len());
        assert_eq!(
            adds,
            head.files.iter().map(|f| f.diff.additions).sum::<usize>()
        );
        assert_eq!(
            dels,
            head.files.iter().map(|f| f.diff.deletions).sum::<usize>()
        );

        // ステージ済みだけ: 未追跡も未ステージ変更も出ない
        let staged = collect_review(&repo, &ReviewBase::Staged, ContextLines::Three, false);
        let sn: Vec<&str> = staged.files.iter().map(|f| f.path.as_str()).collect();
        assert!(sn.contains(&"src/deep/mod.rs"), "{sn:?}");
        assert!(
            !sn.contains(&"untracked.txt"),
            "未追跡は index に無い: {sn:?}"
        );
        assert!(!sn.contains(&"keep.rs"), "未ステージは出ない: {sn:?}");

        // 未ステージだけ: index に上げた分は出ない
        let unstaged = collect_review(&repo, &ReviewBase::Unstaged, ContextLines::Three, false);
        let un: Vec<&str> = unstaged.files.iter().map(|f| f.path.as_str()).collect();
        assert!(un.contains(&"keep.rs"), "{un:?}");
        assert!(!un.contains(&"src/deep/mod.rs"), "{un:?}");
        assert!(
            un.contains(&"untracked.txt"),
            "未追跡は作業ツリー側: {un:?}"
        );

        // 任意リビジョン: HEAD~1 と比べると seed コミットの分も差分に出る
        let rev = collect_review(
            &repo,
            &ReviewBase::Rev("HEAD~1".into()),
            ContextLines::Three,
            false,
        );
        let rn: Vec<&str> = rev.files.iter().map(|f| f.path.as_str()).collect();
        assert!(rev.error.is_none(), "{:?}", rev.error);
        assert!(
            rn.contains(&"moved-dst.txt") || rn.contains(&"moved-src.txt"),
            "{rn:?}"
        );

        // 存在しないリビジョンは静かにエラー文言を返す (panic しない)
        let bad = collect_review(
            &repo,
            &ReviewBase::Rev("no-such-rev-xyz".into()),
            ContextLines::Three,
            false,
        );
        assert!(bad.error.is_some());
        assert!(bad.files.is_empty());

        std::fs::remove_dir_all(&repo).ok();
    }

    /// ステージ → アンステージ → 破棄を実フィクスチャで撃ち、
    /// 引数表どおりに状態が変わることを確かめる (安全な使い捨て repo 内のみ)。
    #[test]
    fn stage_unstage_discard_change_real_state() {
        let Some(repo) = review_repo("mutate") else {
            return;
        };
        std::fs::write(repo.join("keep.rs"), "fn main() { edited(); }\n").expect("edit");
        std::fs::write(repo.join("junk.txt"), "throwaway\n").expect("untracked");

        let run = |args: &[String]| {
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            run_git(&repo, &argv)
                .map(|_| ())
                .map_err(|e| e.text().to_string())
        };

        // ステージ → Staged ベースに出る
        run(&stage_args("keep.rs")).expect("stage");
        let staged = collect_review(&repo, &ReviewBase::Staged, ContextLines::Three, false);
        assert!(staged.files.iter().any(|f| f.path == "keep.rs"));

        // アンステージ → Staged ベースから消える
        run(&unstage_args("keep.rs")).expect("unstage");
        let staged = collect_review(&repo, &ReviewBase::Staged, ContextLines::Three, false);
        assert!(
            !staged.files.iter().any(|f| f.path == "keep.rs"),
            "index から下りた"
        );

        // 破棄 (追跡済み) → 中身が HEAD に戻る
        run(&discard_args("keep.rs", false)).expect("discard tracked");
        assert_eq!(
            std::fs::read_to_string(repo.join("keep.rs")).expect("read"),
            "fn main() {}\n",
            "作業ツリーが HEAD に戻る"
        );

        // 破棄 (未追跡) → ファイルごと消える
        run(&discard_args("junk.txt", true)).expect("discard untracked");
        assert!(!repo.join("junk.txt").exists(), "未追跡ファイルは消える");

        std::fs::remove_dir_all(&repo).ok();
    }

    /// レビュー画面のインラインコメントは既存の diff レンダラのストアを
    /// そのまま使う。パスを鍵にしているので、再収集で添字がずれても
    /// コメントが迷子にならない。
    #[test]
    fn inline_comments_survive_recollection_by_path_key() {
        use crate::diff::{CommentAnchor, CommentSide, DiffCommentStore};
        let mut comments: std::collections::HashMap<String, DiffCommentStore> =
            std::collections::HashMap::new();
        let store = comments.entry("src/deep/mod.rs".to_string()).or_default();
        let anchor = CommentAnchor::new("src/deep/mod.rs", CommentSide::New, 3);
        store.add(anchor.clone(), "// three", "ここは const にしたい");
        assert_eq!(store.actionable_len(), 1);
        assert!(store.prompt().contains("ここは const にしたい"));

        // 収集し直してファイルの並びが変わっても、パス鍵なので同じ位置に残る
        let store = comments.get("src/deep/mod.rs").expect("パス鍵で引ける");
        assert_eq!(store.at(&anchor).len(), 1);
        assert_eq!(store.badge(&anchor), Some((1, false)));
    }

    // ── commit / push / pull / log の引数表 ─────────────────────────

    /// コミットメッセージは**シェルを通らない**ので、引用符も改行も
    /// 絵文字も日本語も、そのまま 1 個の引数として届かなければならない。
    #[test]
    fn commit_args_pass_the_message_verbatim_as_one_argv_element() {
        let msgs = [
            "普通の件名",
            "quote \" と ' を含む",
            "件名\n\n本文の 1 行目\n本文の 2 行目",
            "絵文字 🎉 と タブ\t混じり",
            "-- で始まる怪しい行\n--amend",
            "#1234 を直す", // cleanup で消えてはいけない
            "日本語の件名（全角括弧）と `backtick` と $VAR と ;rm -rf /",
        ];
        for m in msgs {
            let a = commit_args(m, false);
            assert_eq!(a[0], "commit");
            assert!(
                a.contains(&"--cleanup=whitespace".to_string()),
                "commit.cleanup の設定に引きずられない: {a:?}"
            );
            let i = a.iter().position(|s| s == "--message").expect("--message");
            assert_eq!(a[i + 1], m, "メッセージは加工せず 1 要素で渡す");
            assert_eq!(a.len(), i + 2, "メッセージの後ろに何も足さない: {a:?}");
            // シェルを経由しない証拠: クォートもエスケープも一切足していない
            assert!(
                !a[i + 1].contains("\\\"") && !a[i + 1].starts_with('\''),
                "クォート処理を挟んでいない: {:?}",
                a[i + 1]
            );
        }
    }

    #[test]
    fn commit_args_amend_inserts_the_flag_before_the_message() {
        let a = commit_args("直前を直す", true);
        assert_eq!(
            a,
            vec![
                "commit".to_string(),
                "--cleanup=whitespace".into(),
                "--amend".into(),
                "--message".into(),
                "直前を直す".into(),
            ]
        );
        assert!(
            !commit_args("x", false).contains(&"--amend".to_string()),
            "amend でないときは付けない"
        );
    }

    #[test]
    fn commit_blocker_table() {
        // (ステージ数, amend, メッセージ, 止まるか, 何を見ているか)
        let cases: &[(usize, bool, &str, bool, &str)] = &[
            (0, false, "件名", true, "ステージ 0 件ではコミットできない"),
            (1, false, "件名", false, "1 件あれば通す"),
            (
                0,
                true,
                "件名",
                false,
                "amend はステージ 0 件でも意味がある",
            ),
            (3, false, "", true, "空メッセージは止める"),
            (3, false, "   \n\t ", true, "空白だけも空と同じ"),
            (0, false, "", true, "両方欠けていても理由は 1 つ"),
        ];
        for (staged, amend, msg, want_block, why) in cases {
            let got = commit_blocker(*staged, *amend, msg);
            assert_eq!(got.is_some(), *want_block, "{why}");
            if let Some(r) = got {
                assert!(!r.trim().is_empty(), "{why}: 理由を必ず言葉にする");
            }
        }
    }

    #[test]
    fn push_and_pull_arg_tables() {
        assert_eq!(
            push_args("origin", "main", false).expect("ok"),
            vec!["push", "--", "origin", "main"]
        );
        assert_eq!(
            push_args("origin", "feat/x", true).expect("ok"),
            vec!["push", "--set-upstream", "--", "origin", "feat/x"]
        );
        // force は**生成できない** (このアプリのスコープ外)
        for su in [false, true] {
            let a = push_args("origin", "main", su).expect("ok");
            assert!(
                !a.iter().any(|s| s.contains("force") || s == "-f"),
                "force を組み立ててはいけない: {a:?}"
            );
        }
        for (r, b) in [
            ("", "main"),
            ("origin", ""),
            ("-x", "main"),
            ("origin", "-b"),
        ] {
            assert!(push_args(r, b, false).is_err(), "{r:?}/{b:?} は弾く");
        }
        assert_eq!(pull_args(), vec!["pull", "--ff-only"]);
    }

    #[test]
    fn log_args_use_nul_separators_and_paging() {
        let a = log_args(30, 30);
        assert_eq!(a[0], "log");
        assert!(a.contains(&"-z".to_string()), "レコード区切りは NUL: {a:?}");
        assert!(a.contains(&"--skip=30".to_string()), "{a:?}");
        assert!(a.contains(&"--max-count=30".to_string()), "{a:?}");
        let fmt = a.last().expect("format");
        assert!(fmt.starts_with("--pretty=format:"), "{fmt}");
        assert_eq!(fmt.matches("%x00").count(), 4, "5 列 = 区切り 4 個: {fmt}");
        assert!(
            !fmt.contains('|') && !fmt.contains("%x09"),
            "メッセージに入り得る文字を区切りに使わない: {fmt}"
        );
    }

    /// コミットメッセージに改行・タブ・区切りっぽい文字が入っても列がずれない。
    #[test]
    fn parse_log_z_survives_nasty_subjects() {
        let rec = |sha: &str, short: &str, who: &str, t: &str, subj: &str| {
            format!("{sha}\0{short}\0{who}\0{t}\0{subj}")
        };
        let out = [
            rec(
                "a".repeat(40).as_str(),
                "aaaaaaa",
                "山田 太郎",
                "1786209772",
                "タブ\tと | と \" を含む件名 🎉",
            ),
            rec(
                "b".repeat(40).as_str(),
                "bbbbbbb",
                "Ada Lovelace",
                "1700000000",
                "fix: -z を使う",
            ),
        ]
        .join("\0");
        let v = parse_log_z(&out);
        assert_eq!(v.len(), 2, "2 件に割れる: {v:?}");
        assert_eq!(v[0].author, "山田 太郎");
        assert_eq!(v[0].subject, "タブ\tと | と \" を含む件名 🎉");
        assert_eq!(v[0].time, 1786209772);
        assert_eq!(v[1].short, "bbbbbbb");
        assert_eq!(v[1].subject, "fix: -z を使う");
    }

    #[test]
    fn parse_log_z_handles_empty_and_ragged_output() {
        assert!(parse_log_z("").is_empty());
        assert!(
            parse_log_z("\0\0\0").is_empty(),
            "空フィールドだけなら 0 件"
        );
        // 途中で切れたレコードは捨てる (panic しない)
        let half = format!("{}\0short\0who", "c".repeat(40));
        assert!(parse_log_z(&half).is_empty());
    }

    #[test]
    fn parse_left_right_count_reads_behind_then_ahead() {
        assert_eq!(
            parse_left_right_count("2\t5\n"),
            Some((5, 2)),
            "左=behind 右=ahead"
        );
        assert_eq!(parse_left_right_count("0\t0"), Some((0, 0)));
        assert_eq!(parse_left_right_count(""), None);
        assert_eq!(parse_left_right_count("garbage"), None);
    }

    #[test]
    fn staged_count_only_counts_the_index_side() {
        let e = |code: &str, path: &str| ChangeEntry {
            code: code.to_string(),
            path: path.to_string(),
            orig: None,
        };
        let changes = vec![
            e("M ", "staged.rs"),
            e("MM", "both.rs"),
            e(" M", "worktree-only.rs"),
            e("??", "untracked.rs"),
            e("A ", "added.rs"),
            e("R ", "renamed.rs"),
            e("UU", "conflict.rs"),
        ];
        assert_eq!(staged_count(&changes), 5, "index 側に印がある 5 件");
        assert!(!changes[2].staged() && !changes[3].staged());
    }

    #[test]
    fn upstream_suggests_origin_but_never_decides_alone() {
        let head = HeadState::OnBranch("feat/x".into());
        let up = UpstreamInfo {
            name: None,
            remotes: vec!["upstream".into(), "origin".into()],
            ..Default::default()
        };
        assert_eq!(
            up.suggest_push_target(&head),
            Some(("origin".to_string(), "feat/x".to_string())),
            "origin があれば origin を推す"
        );
        let only = UpstreamInfo {
            name: None,
            remotes: vec!["gitlab".into()],
            ..Default::default()
        };
        assert_eq!(
            only.suggest_push_target(&head),
            Some(("gitlab".to_string(), "feat/x".to_string()))
        );
        // upstream が既にある / detached / リモート無し では提案しない
        let has = UpstreamInfo {
            name: Some("origin/main".into()),
            remotes: vec!["origin".into()],
            ..Default::default()
        };
        assert_eq!(has.suggest_push_target(&head), None);
        assert_eq!(
            up.suggest_push_target(&HeadState::Detached("abc1234".into())),
            None
        );
        let none = UpstreamInfo::default();
        assert_eq!(none.suggest_push_target(&head), None);
    }

    #[test]
    fn hunk_ops_depend_on_the_base_and_never_lie() {
        use crate::diff::HunkOp;
        assert_eq!(
            hunk_ops_for(&ReviewBase::Unstaged, false, false),
            vec![HunkOp::Stage, HunkOp::Discard]
        );
        assert_eq!(
            hunk_ops_for(&ReviewBase::Staged, false, false),
            vec![HunkOp::Unstage]
        );
        // 作業ツリー vs HEAD は index と一致する保証が無いので出さない
        assert!(hunk_ops_for(&ReviewBase::Head, false, false).is_empty());
        assert!(hunk_ops_for(&ReviewBase::Rev("main".into()), false, false).is_empty());
        // 空白無視の差分は原理的に適用できない / 未追跡はハンクが無い
        assert!(hunk_ops_for(&ReviewBase::Unstaged, true, false).is_empty());
        assert!(hunk_ops_for(&ReviewBase::Unstaged, false, true).is_empty());
    }

    #[test]
    fn apply_patch_arg_table() {
        assert_eq!(
            apply_patch_args(true, false),
            vec!["apply", "--cached", "--whitespace=nowarn", "-"]
        );
        assert_eq!(
            apply_patch_args(true, true),
            vec!["apply", "--cached", "--reverse", "--whitespace=nowarn", "-"]
        );
        assert_eq!(
            apply_patch_args(false, true),
            vec!["apply", "--reverse", "--whitespace=nowarn", "-"]
        );
        for (c, r) in [(true, false), (true, true), (false, true)] {
            let a = apply_patch_args(c, r);
            assert_eq!(
                a.last().map(String::as_str),
                Some("-"),
                "パッチは標準入力から"
            );
        }
    }

    /// **UI から到達できない実装は未完成**。コミット / 履歴 / ハンク操作が
    /// 実際に描画経路へ繋がっていることをソースで固定する
    /// (`include_str!` は CRLF チェックアウト対策で必ず正規化する)。
    #[test]
    fn コミット導線とハンク操作が画面に繋がっている() {
        let panel = include_str!("git_panel.rs").replace("\r\n", "\n");
        assert!(
            panel.contains("self.commit_ui(ui, theme, info, busy, &mut req);"),
            "コミット欄を描いていない (描かないと画面に出ない)"
        );
        assert!(
            panel.contains("self.history_ui(ui, theme, info, actions);"),
            "履歴を描いていない"
        );
        assert!(
            panel.contains("crate::diff::diff_ui_with_hunk_actions("),
            "ハンク操作付きの差分レンダラを呼んでいない"
        );
        assert!(
            panel.contains("self.spawn_history(&ctx, 0);"),
            "履歴の初回読み込みが仕込まれていない"
        );
        let app = include_str!("app.rs").replace("\r\n", "\n");
        assert!(
            app.contains("self.git_panel.ui(ui, &theme, &mut git_actions);"),
            "GitPanel を描いていない"
        );
        assert!(
            app.contains("if let Some((top, sha)) = git_actions.open_commit {"),
            "履歴クリックのコミット差分を app 側で受けていない"
        );
        assert!(
            app.contains("self.open_commit_diff_at(&top, &sha);"),
            "既存の commit_diff → commit_diff_ui の経路へ流していない"
        );
    }

    // ── 実 git フィクスチャ: コミット導線とハンクステージ ───────────

    /// ステージ → メッセージ → コミット → 履歴に出る、が引数表だけで完結する。
    /// **`git push` は絶対に走らせない** (ネットワークへ出ない)。
    #[test]
    fn commit_flow_lands_in_the_log() {
        let Some(repo) = review_repo("commit-flow") else {
            return; // git が無い環境ではスキップ
        };
        std::fs::write(repo.join("keep.rs"), "fn main() { changed(); }\n").expect("edit");
        // ステージ前は 0 件 → コミットできない
        let before =
            parse_status_porcelain(&run_git(&repo, &["status", "--porcelain=v1"]).expect("status"));
        assert_eq!(staged_count(&before), 0);
        assert!(commit_blocker(staged_count(&before), false, "件名").is_some());

        let sa = stage_args("keep.rs");
        let argv: Vec<&str> = sa.iter().map(String::as_str).collect();
        run_git(&repo, &argv).expect("stage");
        let after =
            parse_status_porcelain(&run_git(&repo, &["status", "--porcelain=v1"]).expect("status"));
        assert_eq!(staged_count(&after), 1, "ステージ済み 1 件");
        assert!(commit_blocker(staged_count(&after), false, "件名").is_none());

        // 引用符 / 改行 / タブ / 絵文字 / 日本語を全部入れる
        let msg = "修正: \"引用符\"\tと絵文字 🎉\n\n本文の 1 行目\n本文の 2 行目";
        let ca = commit_args(msg, false);
        let argv: Vec<&str> = ca.iter().map(String::as_str).collect();
        run_git(&repo, &["config", "user.name", "zv"]).expect("name");
        run_git(&repo, &["config", "user.email", "zv@example.com"]).expect("email");
        run_git(&repo, &argv).expect("commit");

        let la = log_args(0, HISTORY_PAGE);
        let argv: Vec<&str> = la.iter().map(String::as_str).collect();
        let log = parse_log_z(&run_git(&repo, &argv).expect("log"));
        assert_eq!(log.len(), 2, "init + 今のコミット: {log:?}");
        // %s は「1 行目 = 件名 / 空行 / 本文」の慣習どおり件名だけを返す。
        // タブも引用符も絵文字も列をずらさない (区切りが NUL だから)。
        assert_eq!(log[0].subject, "修正: \"引用符\"\tと絵文字 🎉");
        assert_eq!(log[0].sha.len(), 40);
        assert!(log[0].time > 0, "author date が取れる");

        // amend: 直前のメッセージを読み直して書き換える
        let head_msg =
            run_git(&repo, &["log", "-1", "--no-color", "--pretty=format:%B"]).expect("%B");
        assert!(
            head_msg.contains("本文の 2 行目"),
            "%B は全文: {head_msg:?}"
        );
        let aa = commit_args("件名を書き直した", true);
        let argv: Vec<&str> = aa.iter().map(String::as_str).collect();
        run_git(&repo, &argv).expect("amend");
        let log = parse_log_z(
            &run_git(
                &repo,
                &[
                    "log",
                    "--no-color",
                    "-z",
                    "--max-count=30",
                    "--pretty=format:%H%x00%h%x00%an%x00%at%x00%s",
                ],
            )
            .expect("log"),
        );
        assert_eq!(log.len(), 2, "amend でコミットは増えない");
        assert_eq!(log[0].subject, "件名を書き直した");

        // ページング: 2 件目以降を skip で取り直せる
        let la = log_args(1, HISTORY_PAGE);
        let argv: Vec<&str> = la.iter().map(String::as_str).collect();
        let page2 = parse_log_z(&run_git(&repo, &argv).expect("log"));
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].sha, log[1].sha, "続きは重複しない");

        std::fs::remove_dir_all(&repo).ok();
    }

    /// 組み立てたパッチが **実際に `git apply` へ通る**こと。
    /// ここが通らないとハンクステージは絵に描いた餅になる。
    #[test]
    fn hunk_patch_applies_to_the_index_and_back() {
        let Some(repo) = review_repo("hunk-apply") else {
            return;
        };
        // 離れた 2 か所を変える → ハンクが 2 本になる
        let base: String = (1..=40).map(|i| format!("line{i}\n")).collect();
        std::fs::write(repo.join("many.txt"), &base).expect("seed");
        run_git(&repo, &["add", "-A"]).expect("add");
        run_git(&repo, &["config", "user.name", "zv"]).expect("name");
        run_git(&repo, &["config", "user.email", "zv@example.com"]).expect("email");
        run_git(&repo, &["commit", "--quiet", "-m", "seed"]).expect("commit");
        let edited = base
            .replace("line3\n", "LINE3\n")
            .replace("line30\n", "LINE30\n");
        std::fs::write(repo.join("many.txt"), &edited).expect("edit");

        // 未ステージの差分 (作業ツリー vs index) を取る
        let data = collect_review(&repo, &ReviewBase::Unstaged, ContextLines::Three, false);
        let f = data
            .files
            .iter()
            .find(|f| f.path == "many.txt")
            .expect("many.txt の差分");
        assert_eq!(f.diff.hunks.len(), 2, "ハンクは 2 本");

        // 1 本目だけステージ
        let patch = crate::diff::build_hunk_patch(&f.diff, 0).expect("パッチ");
        let args = apply_patch_args(true, false);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git_stdin(&repo, &argv, &patch).expect("apply --cached");

        let staged = run_git(&repo, &["diff", "--cached", "--no-color", "-U0"]).expect("staged");
        assert!(
            staged.contains("+LINE3\n"),
            "1 本目が index に載った:\n{staged}"
        );
        assert!(
            !staged.contains("+LINE30"),
            "2 本目まで載せてはいけない:\n{staged}"
        );
        // 作業ツリーは両方変わったまま
        let wt = std::fs::read_to_string(repo.join("many.txt")).expect("read");
        assert!(wt.contains("LINE3\n") && wt.contains("LINE30\n"));

        // ステージ済みの差分からアンステージ (--cached --reverse)
        let staged_data = collect_review(&repo, &ReviewBase::Staged, ContextLines::Three, false);
        let sf = staged_data
            .files
            .iter()
            .find(|f| f.path == "many.txt")
            .expect("ステージ済み差分");
        let patch = crate::diff::build_hunk_patch(&sf.diff, 0).expect("パッチ");
        let args = apply_patch_args(true, true);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git_stdin(&repo, &argv, &patch).expect("apply --cached --reverse");
        let staged = run_git(&repo, &["diff", "--cached", "--name-only"]).expect("staged");
        assert!(staged.trim().is_empty(), "index が空に戻る: {staged:?}");

        // 破棄 (作業ツリーだけ巻き戻す)
        let data = collect_review(&repo, &ReviewBase::Unstaged, ContextLines::Three, false);
        let f = data
            .files
            .iter()
            .find(|f| f.path == "many.txt")
            .expect("差分");
        let patch = crate::diff::build_hunk_patch(&f.diff, 1).expect("パッチ");
        let args = apply_patch_args(false, true);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git_stdin(&repo, &argv, &patch).expect("apply --reverse");
        let wt = std::fs::read_to_string(repo.join("many.txt")).expect("read");
        assert!(wt.contains("LINE3\n"), "1 本目は残る");
        assert!(!wt.contains("LINE30\n"), "2 本目だけ巻き戻る");

        std::fs::remove_dir_all(&repo).ok();
    }

    /// 末尾に改行が無いファイル / CRLF のファイル / 新規ファイルの
    /// パッチが実際に `git apply --cached` へ通ること。
    #[test]
    fn hunk_patch_applies_for_no_newline_crlf_and_new_files() {
        let Some(repo) = review_repo("hunk-edge") else {
            return;
        };
        run_git(&repo, &["config", "user.name", "zv"]).expect("name");
        run_git(&repo, &["config", "user.email", "zv@example.com"]).expect("email");
        // 末尾に改行が無いファイルと CRLF のファイルを種として置く
        std::fs::write(repo.join("nn.txt"), "p\nq").expect("nn");
        std::fs::write(repo.join("crlf.txt"), "l1\r\nl2\r\n").expect("crlf");
        run_git(&repo, &["add", "-A"]).expect("add");
        run_git(&repo, &["commit", "--quiet", "-m", "edge seed"]).expect("commit");

        std::fs::write(repo.join("nn.txt"), "p\nQ").expect("nn edit");
        std::fs::write(repo.join("crlf.txt"), "l1\r\nL2\r\n").expect("crlf edit");
        std::fs::write(repo.join("brand-new.txt"), "n1\nn2\n").expect("new");
        run_git(&repo, &["add", "--intent-to-add", "brand-new.txt"]).expect("intent");

        let data = collect_review(&repo, &ReviewBase::Unstaged, ContextLines::Three, false);
        for name in ["nn.txt", "crlf.txt", "brand-new.txt"] {
            let f = data
                .files
                .iter()
                .find(|f| f.path == name)
                .unwrap_or_else(|| panic!("{name} の差分が無い"));
            let patch = crate::diff::build_hunk_patch(&f.diff, 0)
                .unwrap_or_else(|| panic!("{name} のパッチ"));
            let args = apply_patch_args(true, false);
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            run_git_stdin(&repo, &argv, &patch)
                .unwrap_or_else(|e| panic!("{name}: apply が通らない: {}\n{patch}", e.text()));
        }
        // index の中身がバイト単位で作業ツリーと一致する
        for name in ["nn.txt", "crlf.txt", "brand-new.txt"] {
            let idx = run_git(&repo, &["show", &format!(":{name}")]).expect("show");
            let wt = std::fs::read_to_string(repo.join(name)).expect("read");
            assert_eq!(idx, wt, "{name}: index と作業ツリーが一致する");
        }

        std::fs::remove_dir_all(&repo).ok();
    }
}
