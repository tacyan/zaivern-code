//! Git パネル (左サイドバー用)。
//!
//! ブランチ / worktree / 変更ファイルを一覧し、**安全で取り消せる操作だけ**を提供する。
//! commit / push / ブランチ削除 / worktree 削除 / stage / reset / merge / rebase は
//! 意図的にスコープ外。
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
            (None, false) => "(不明)".to_string(),
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
}

/// 収集済みリポジトリ情報。
#[derive(Clone, Default)]
pub struct RepoInfo {
    pub toplevel: PathBuf,
    pub head: HeadState,
    pub branches: BranchList,
    pub worktrees: Vec<WorktreeEntry>,
    pub changes: Vec<ChangeEntry>,
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
    let mut c = Command::new("git");
    c.arg("-C").arg(ws).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // GUI アプリからコンソール窓を出さない
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let out = c.output().map_err(|e| RunErr::Spawn(e.to_string()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(RunErr::Failed(if err.is_empty() {
            format!("git {} が失敗しました", args.join(" "))
        } else {
            err
        }));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// ワークスペースの git 情報をまとめて集める (バックグラウンドスレッドで呼ぶ)。
fn collect(ws: &Path) -> RepoState {
    let toplevel = match run_git(ws, &["rev-parse", "--show-toplevel"]) {
        Ok(s) => PathBuf::from(s.trim()),
        Err(RunErr::Spawn(_)) => {
            return RepoState::Unavailable("git コマンドが見つかりません".into());
        }
        Err(RunErr::Failed(_)) => {
            return RepoState::Unavailable("ここは git リポジトリではありません".into());
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

    let worktrees = run_git(ws, &["worktree", "list", "--porcelain"])
        .map(|s| parse_worktree_porcelain(&s))
        .unwrap_or_default();

    let changes = run_git(ws, &["status", "--porcelain=v1"])
        .map(|s| parse_status_porcelain(&s))
        .unwrap_or_default();

    RepoState::Ready(Box::new(RepoInfo {
        toplevel,
        head,
        branches,
        worktrees,
        changes,
    }))
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
        if line.len() < 4 || !line.is_char_boundary(3) {
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
        return Err("名前を入力してください".into());
    }
    if n.starts_with('-') {
        return Err("名前を - で始めることはできません".into());
    }
    if n.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("名前に空白や制御文字は使えません".into());
    }
    if n.contains("..") || n.contains("@{") {
        return Err("名前に .. や @{ は使えません".into());
    }
    if n.chars().any(|c| matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')) {
        return Err("名前に ~ ^ : ? * [ \\ は使えません".into());
    }
    if n.starts_with('/') || n.ends_with('/') || n.ends_with(".lock") {
        return Err("名前の先頭/末尾が不正です".into());
    }
    Ok(n.to_string())
}

/// worktree の入力を検証する。ブランチ名よりは緩く、絶対パスも許す。
pub fn validate_worktree_input(input: &str) -> Result<String, String> {
    let n = input.trim();
    if n.is_empty() {
        return Err("worktree 名を入力してください".into());
    }
    if n.starts_with('-') {
        return Err("名前を - で始めることはできません".into());
    }
    if n.chars().any(char::is_control) {
        return Err("名前に制御文字は使えません".into());
    }
    if n.contains("..") {
        return Err("名前に .. は使えません".into());
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
        }
    }

    pub fn set_workspace(&mut self, ws: PathBuf) {
        if self.workspace != ws {
            self.workspace = ws;
            self.state = RepoState::Loading;
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
                    ui.label(RichText::new("読み込み中…").color(theme.text_dim).small());
                });
            }
            RepoState::Unavailable(msg) => {
                ui.label(RichText::new(msg).color(theme.text_dim).small());
            }
            RepoState::Ready(info) => {
                self.head_ui(ui, theme, info);
                ui.separator();
                self.branches_ui(ui, theme, info, busy, &mut req);
                ui.separator();
                self.worktrees_ui(ui, theme, info, busy, actions, &mut req);
                ui.separator();
                self.changes_ui(ui, theme, info);
            }
        }

        if busy && !self.job_label.is_empty() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(11.0));
                ui.label(
                    RichText::new(format!("{} 実行中…", self.job_label))
                        .color(theme.text_dim)
                        .small(),
                );
            });
        }

        self.state = state;

        if let Some(job) = req {
            self.spawn_job(&ctx, job, actions);
        }
    }

    // -- 各セクション -------------------------------------------------------

    fn header_ui(&mut self, ui: &mut egui::Ui, theme: &Theme, busy: bool) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Git").strong().color(theme.text));
            if self.pending.is_some() {
                ui.add(egui::Spinner::new().size(11.0));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(!busy, egui::Button::new("⟳").small())
                    .on_hover_text("リフレッシュ")
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
                .on_hover_text("ブランチから外れています。作業前にブランチを作るか切り替えてください");
            }
            HeadState::Unknown => {
                ui.label(
                    RichText::new("HEAD 不明 (まだコミットがないかもしれません)")
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
            RichText::new(format!("ブランチ ({})", info.branches.local.len()))
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
                    let resp = ui.add_enabled(
                        !busy && !b.current,
                        egui::Button::new(label).frame(false),
                    );
                    let resp = if b.current {
                        resp.on_hover_text("現在のブランチ")
                    } else if b.other_worktree {
                        resp.on_hover_text("別の worktree で使用中。切替は git が判断します")
                    } else {
                        resp.on_hover_text("クリックで切り替え (git checkout)")
                    };
                    if resp.clicked() {
                        *req = Some(Job::Checkout(b.name.clone()));
                    }
                });
            }
            if info.branches.local.is_empty() {
                ui.label(
                    RichText::new("ローカルブランチがありません")
                        .color(theme.text_dim)
                        .small(),
                );
            }

            // 新規ブランチ
            ui.horizontal(|ui| {
                let te = egui::TextEdit::singleline(&mut self.new_branch_input)
                    .desired_width(f32::INFINITY)
                    .hint_text("新しいブランチ名");
                let resp = ui.add_enabled(!busy, te);
                let enter =
                    resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter && !self.new_branch_input.trim().is_empty() {
                    *req = Some(Job::NewBranch(self.new_branch_input.clone()));
                }
            });
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !busy && !self.new_branch_input.trim().is_empty(),
                        egui::Button::new(RichText::new("＋ ブランチ作成").small()),
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
                    RichText::new(format!("リモート追跡 ({})", info.branches.remote.len()))
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
                let is_current = same_path(&w.path, &info.toplevel);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if is_current { "●" } else { "○" })
                            .color(if is_current { theme.accent } else { theme.text_dim })
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
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if ui
                                .add_enabled(
                                    !is_current && !w.bare,
                                    egui::Button::new(RichText::new("開く").small()),
                                )
                                .on_hover_text("この worktree をワークスペースとして開く")
                                .clicked()
                            {
                                actions.open_path = Some(w.path.clone());
                            }
                        },
                    );
                });
            }

            // worktree 作成
            ui.horizontal(|ui| {
                let te = egui::TextEdit::singleline(&mut self.worktree_input)
                    .desired_width(f32::INFINITY)
                    .hint_text("新しい worktree 名 / 絶対パス");
                let _ = ui.add_enabled(!busy, te);
            });
            ui.checkbox(&mut self.worktree_new_branch, RichText::new("同名のブランチも作る").small());

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
                    egui::Button::new(RichText::new("＋ worktree 作成").small()),
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
            RichText::new(format!("変更ファイル ({})", info.changes.len()))
                .color(theme.text)
                .small(),
        )
        .id_salt("zv_git_changes")
        .default_open(true)
        .show(ui, |ui| {
            if info.changes.is_empty() {
                ui.label(
                    RichText::new("変更はありません")
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
        if let Some(rx) = &self.job {
            match rx.try_recv() {
                Ok(msg) => {
                    actions.toast = Some(msg);
                    self.job = None;
                    self.job_label.clear();
                    // 変更が入ったのでキャッシュを捨てる
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
                self.state = RepoState::Unavailable(format!("git 情報を取得できません: {e}"));
            }
        }
    }

    /// 変更系コマンドを別スレッドで走らせる。UI は絶対にブロックしない。
    fn spawn_job(&mut self, ctx: &egui::Context, job: Job, actions: &mut GitActions) {
        if self.job.is_some() {
            return;
        }
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
                    format!("ブランチ作成 {b}"),
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
                    actions.toast = Some(("worktree のパスが不正です".into(), false));
                    return;
                }
                let mut args: Vec<String> =
                    vec!["worktree".into(), "add".into()];
                if let Some(b) = branch {
                    args.push("-b".into());
                    args.push(b);
                }
                args.push(p.clone());
                (format!("worktree 作成 {p}"), args)
            }
            Job::Fetch => (
                "fetch".to_string(),
                vec!["fetch".into(), "--all".into(), "--prune".into()],
            ),
        };

        let (tx, rx) = mpsc::channel();
        let ws = self.workspace.clone();
        let ctx2 = ctx.clone();
        let label2 = label.clone();
        let spawned = std::thread::Builder::new()
            .name("zv-git-job".into())
            .spawn(move || {
                let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                // stderr は加工せずそのまま伝える (git の拒否理由を握り潰さない)
                let msg = match run_git(&ws, &argv) {
                    Ok(_) => (format!("{label2} 完了"), true),
                    Err(e) => (format!("{label2} 失敗: {}", e.text()), false),
                };
                let _ = tx.send(msg);
                ctx2.request_repaint();
            });
        match spawned {
            Ok(_) => {
                self.job = Some(rx);
                self.job_label = label;
                // 入力欄は投げたら空にする
                self.new_branch_input.clear();
                self.worktree_input.clear();
            }
            Err(e) => {
                actions.toast = Some((format!("git を起動できません: {e}"), false));
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
        let out = concat!(
            "* (HEAD detached at 2f14c3e)\n",
            "  main\n",
            "  develop\n",
        );
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
        assert_eq!(validate_branch_name("日本語ブランチ").unwrap(), "日本語ブランチ");
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
}
