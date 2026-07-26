//! プロンプト・ファンアウトレース — 1 つのプロンプトを複数のエージェントプリセットに
//! 同時に取り組ませ、成果 (差分) を見比べて勝者だけを採用する。
//!
//! 流れ:
//! 1. レース開始 — racer ごとに独立した git worktree を切る (ブランチ
//!    `race/<slug>-<n>`)。作業ツリーが汚れていたら開始を拒否する
//!    (勝者ブランチを後でマージで戻すため、ベースは綺麗でなければならない)。
//! 2. 各 worktree を cwd にしてエージェントを起動し、プロンプトを自動投入する
//!    (投入は app.rs の pending_prompts — Issue 着手フローと同じ配達機構)。
//! 3. ダッシュボード — 各 racer の状態とベースとの差分量を一覧し、[Diff] で全文差分、
//!    [採用] でベースブランチへのマージ、[破棄] で worktree + ブランチの削除を行う。
//!
//! 設計上の要点:
//! - git は**すべて明示のリポジトリパス** (`git -C <repo>`) に対して実行し、
//!   cwd には一切依存しない。出力判定のため LC_ALL=C で英語メッセージに固定する。
//! - 差分量の収集は git_panel と同じ「別スレッド + TTL + try_recv」方式で、
//!   UI スレッドでは git を待たない (開始・採用・破棄はユーザーの単発操作なので
//!   Issue 着手フローと同様に同期実行を許す)。
//! - worktree の置き場はリポジトリの隣 (`<repo>-race-<slug>-<n>`)。ただしリポジトリ
//!   自身が `.claude/worktrees` 配下 (Claude セッションの管理領域) にある場合は
//!   そこを汚さず `std::env::temp_dir()/zaivern-races` へ退避する。
//! - [破棄] は未コミット変更のある worktree を黙って消さない。git 自身が拒否し、
//!   UI の確認フラグ (`confirm_discard`) を経た 2 度目の操作だけが --force を付ける。
//! - [採用] はコンフリクトで止まったら `merge --abort` で巻き戻し、エラーを見せる。
//!   強行はしない。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};

use crate::diff::{self, FileDiff};
use crate::i18n::{tr, trf};
use crate::theme::Theme;

/// レースの最小 / 最大の racer 数。
pub const MIN_RACERS: usize = 2;
pub const MAX_RACERS: usize = 4;

/// ブランチ名に使うスラグの最大長 (ASCII 前提)。
const SLUG_MAX: usize = 24;

/// 差分量ポーリングの間隔。
const STAT_TTL: Duration = Duration::from_secs(4);

/// パース済み差分キャッシュの上限 (panels.rs の PR 差分タブと同じ流儀)。
const DIFF_CACHE_CAP: usize = 16;

// ---------------------------------------------------------------------------
// 純粋ロジック: スラグ / ブランチ名 / パース
// ---------------------------------------------------------------------------

/// プロンプトからブランチ用スラグを作る。ASCII 英数字だけを残し、他は `-` に
/// 潰して連結する。日本語だけのプロンプトのように何も残らない場合は "race"。
pub fn slugify_prompt(prompt: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = true; // 先頭のダッシュを抑止
    for c in prompt.chars() {
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
        "race".to_string()
    } else {
        out
    }
}

/// racer `i` (1 始まり) のブランチ名。
pub fn race_branch(slug: &str, i: usize) -> String {
    format!("race/{slug}-{i}")
}

/// worktree のフォルダ名 (リポジトリの隣に置く)。
pub fn worktree_dir_name(repo_name: &str, slug: &str, i: usize) -> String {
    format!("{repo_name}-race-{slug}-{i}")
}

/// `taken` が false を返すスラグが見つかるまで `-r2`, `-r3`… を足していく。
/// `taken` は「そのスラグで racer 1..=n のブランチ名 or フォルダのどれかが
/// 既に存在するか」を答える。
pub fn unique_slug(base: &str, mut taken: impl FnMut(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    let mut k: u64 = 2;
    loop {
        let cand = format!("{base}-r{k}");
        if !taken(&cand) {
            return cand;
        }
        k += 1;
    }
}

/// レース worktree の置き場。基本はリポジトリの隣 (親フォルダ)。
/// リポジトリが `.claude/worktrees` 配下 (Claude セッションの管理領域) にある、
/// または親が取れない場合は `temp_dir()/zaivern-races` へ退避する。
pub fn race_worktree_base(repo: &Path) -> PathBuf {
    let inside_claude_worktrees = repo.ancestors().any(|a| {
        a.file_name().is_some_and(|n| n == "worktrees")
            && a.parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n == ".claude")
    });
    match repo.parent() {
        Some(p) if !inside_claude_worktrees && p != Path::new("") => p.to_path_buf(),
        _ => std::env::temp_dir().join("zaivern-races"),
    }
}

/// `git worktree remove` の引数列。--force は UI の確認フラグを経た時だけ付く。
pub fn worktree_remove_args(dir: &str, force: bool) -> Vec<String> {
    let mut args = vec!["worktree".to_string(), "remove".to_string()];
    if force {
        args.push("--force".to_string());
    }
    args.push(dir.to_string());
    args
}

/// racer に自動投入するプロンプト本文。ユーザーの指示 + レースの約束事。
pub fn build_race_prompt(prompt: &str, branch: &str) -> String {
    format!(
        "{prompt}\n\n(これは複数エージェントの並走レースです。この作業ツリーはあなた専用の\
         ブランチ {branch} です。実装が終わったらテストを通してコミットしてください。)"
    )
}

/// 変更量。`git diff --shortstat` + 未追跡ファイル数。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct DiffStat {
    pub files: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub untracked: usize,
}

/// `git diff --shortstat` の 1 行 (" 3 files changed, 10 insertions(+), 2 deletions(-)")
/// をパースする。空文字 (差分なし) はすべて 0。
pub fn parse_shortstat(out: &str) -> DiffStat {
    let mut st = DiffStat::default();
    for seg in out.split(',') {
        let num: usize = seg
            .split_whitespace()
            .next()
            .and_then(|t| t.parse().ok())
            .unwrap_or(0);
        if seg.contains("file") {
            st.files = num;
        } else if seg.contains("insertion") {
            st.insertions = num;
        } else if seg.contains("deletion") {
            st.deletions = num;
        }
    }
    st
}

/// マージ結果の種別。`git merge` の標準出力 (LC_ALL=C) から判定する。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MergeKind {
    FastForward,
    Merge,
    UpToDate,
}

pub fn parse_merge_kind(out: &str) -> MergeKind {
    if out.contains("Already up to date") {
        MergeKind::UpToDate
    } else if out.contains("Fast-forward") {
        MergeKind::FastForward
    } else {
        MergeKind::Merge
    }
}

// ---------------------------------------------------------------------------
// レースの状態
// ---------------------------------------------------------------------------

/// racer 1 体の状態。Adopted / Discarded / Error は終端で、決して覆らない。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RacerStatus {
    /// worktree は切ったがエージェントはまだ起動していない
    Preparing,
    Running,
    Exited,
    /// ベースブランチへマージ済み
    Adopted,
    /// worktree + ブランチを削除済み
    Discarded,
    /// 起動失敗など (本文は日本語メッセージ)
    Error(String),
}

impl RacerStatus {
    /// もう遷移しない状態か。
    pub fn settled(&self) -> bool {
        matches!(
            self,
            RacerStatus::Adopted | RacerStatus::Discarded | RacerStatus::Error(_)
        )
    }

    /// セッションの生存情報から次の状態を決める。
    /// `running`: `Some(true)` = 走行中 / `Some(false)` = 終了 /
    /// `None` = セッションが見つからない (タブごと閉じられた)。
    pub fn next(&self, running: Option<bool>) -> RacerStatus {
        if self.settled() {
            return self.clone();
        }
        match running {
            Some(true) => RacerStatus::Running,
            Some(false) | None => RacerStatus::Exited,
        }
    }
}

/// 競走者 1 体。
#[derive(Clone, Debug)]
pub struct Racer {
    pub preset_name: String,
    pub icon: String,
    pub branch: String,
    pub dir: PathBuf,
    /// 起動したエージェントセッションの id (起動失敗なら None のまま)
    pub session_id: Option<u64>,
    pub status: RacerStatus,
    pub stat: Option<DiffStat>,
    /// [破棄] の 2 段確認。未コミット変更で git が削除を拒否した後に立ち、
    /// 次の [⚠ 強制破棄] だけが --force を付ける。
    pub confirm_discard: bool,
}

/// 進行中のレース 1 本。
#[derive(Clone, Debug)]
pub struct Race {
    /// レースを始めたリポジトリのトップレベル (worktree/マージ操作の基準)
    pub repo: PathBuf,
    /// レース開始時にチェックアウトされていたブランチ (採用のマージ先)
    pub base_branch: String,
    /// レース開始時の HEAD (差分の比較基準)
    pub base_commit: String,
    pub prompt: String,
    pub racers: Vec<Racer>,
}

// ---------------------------------------------------------------------------
// git 実行 (すべて明示パス・LC_ALL=C)
// ---------------------------------------------------------------------------

/// `git -C <repo> <args...>` を窓なしで実行する。出力文言の判定があるので
/// LC_ALL=C で英語に固定する (procx が PATH も面倒を見る)。
fn run_git(repo: &Path, args: &[&str]) -> Result<String, String> {
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

/// レースを開始する: リポジトリ検証 → 汚れチェック → racer ごとの worktree 作成。
/// `presets` は (アイコン, プリセット名) — panels.rs の Issue 着手メニューと同じ形。
/// エージェントの起動は呼び出し側 (app.rs) が行う。
pub fn start_race(root: &Path, prompt: &str, presets: &[(String, String)]) -> Result<Race, String> {
    let n = presets.len();
    if !(MIN_RACERS..=MAX_RACERS).contains(&n) {
        return Err(trf(
            "レースは {min}〜{max} 体で行います",
            &[("min", MIN_RACERS.to_string()), ("max", MAX_RACERS.to_string())],
        ));
    }
    let repo = PathBuf::from(
        run_git(root, &["rev-parse", "--show-toplevel"])
            .map_err(|e| trf("git リポジトリではありません: {e}", &[("e", e)]))?,
    );
    // 汚れた作業ツリーでは開始しない (勝者を後でマージで戻すため)。
    // 未追跡ファイルはマージの妨げにならないので -uno で無視する。
    let dirty = run_git(&repo, &["status", "--porcelain", "--untracked-files=no"])?;
    if !dirty.is_empty() {
        return Err(tr(
            "作業ツリーに未コミットの変更があります — コミットまたは stash してからレースを開始してください",
        ));
    }
    let base_branch = run_git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if base_branch == "HEAD" {
        return Err(tr(
            "detached HEAD ではレースを開始できません (ブランチをチェックアウトしてください)",
        ));
    }
    let base_commit = run_git(&repo, &["rev-parse", "HEAD"])?;
    let branches: Vec<String> = run_git(
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
    let base_dir = race_worktree_base(&repo);
    let slug = unique_slug(&slugify_prompt(prompt), |cand| {
        (1..=n).any(|i| {
            branches.iter().any(|b| b == &race_branch(cand, i))
                || base_dir.join(worktree_dir_name(&repo_name, cand, i)).exists()
        })
    });
    std::fs::create_dir_all(&base_dir)
        .map_err(|e| trf("worktree の置き場を作れません: {e}", &[("e", e.to_string())]))?;

    let mut racers: Vec<Racer> = Vec::with_capacity(n);
    for (i, (icon, name)) in presets.iter().enumerate() {
        let idx = i + 1;
        let branch = race_branch(&slug, idx);
        let dir = base_dir.join(worktree_dir_name(&repo_name, &slug, idx));
        let dir_s = dir.to_string_lossy().into_owned();
        if let Err(e) = run_git(
            &repo,
            &["worktree", "add", "-b", &branch, &dir_s, &base_commit],
        ) {
            // 作りかけを巻き戻してから失敗を返す (途中まで作って散らかさない)。
            for r in &racers {
                let _ = run_git(
                    &repo,
                    &["worktree", "remove", "--force", &r.dir.to_string_lossy()],
                );
                let _ = run_git(&repo, &["branch", "-D", &r.branch]);
            }
            return Err(trf(
                "worktree を作成できません ({branch}): {e}",
                &[("branch", branch), ("e", e)],
            ));
        }
        racers.push(Racer {
            preset_name: name.clone(),
            icon: icon.clone(),
            branch,
            dir,
            session_id: None,
            status: RacerStatus::Preparing,
            stat: None,
            confirm_discard: false,
        });
    }
    Ok(Race {
        repo,
        base_branch,
        base_commit,
        prompt: prompt.to_string(),
        racers,
    })
}

/// racer の変更量を集める。`git diff <base>` は「作業ツリー vs base」なので
/// コミット済み + 未コミットの両方が載る。未追跡ファイル数は status から別集計。
fn collect_stat(dir: &Path, base_commit: &str) -> Result<DiffStat, String> {
    let short = run_git(dir, &["diff", "--shortstat", base_commit])?;
    let mut st = parse_shortstat(&short);
    let status = run_git(dir, &["status", "--porcelain"])?;
    st.untracked = status.lines().filter(|l| l.starts_with("??")).count();
    Ok(st)
}

/// racer のブランチをベースブランチへマージする (fast-forward or 通常マージ)。
/// コンフリクトなら `merge --abort` で巻き戻してエラーを返す — 強行はしない。
pub fn adopt_racer(race: &Race, idx: usize) -> Result<String, String> {
    let racer = race
        .racers
        .get(idx)
        .ok_or_else(|| tr("racer が見つかりません"))?;
    // 未コミットの成果はマージに載らない (worktree に置き去りになる) ので拒否。
    let racer_dirty = run_git(&racer.dir, &["status", "--porcelain"])?;
    if !racer_dirty.is_empty() {
        return Err(trf(
            "{branch} に未コミットの変更があります — エージェントにコミットさせてから採用してください",
            &[("branch", racer.branch.clone())],
        ));
    }
    let cur = run_git(&race.repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if cur != race.base_branch {
        return Err(trf(
            "ベースが {base} から {cur} へ移っています — {base} をチェックアウトしてから採用してください",
            &[("base", race.base_branch.clone()), ("cur", cur)],
        ));
    }
    let base_dirty = run_git(&race.repo, &["status", "--porcelain", "--untracked-files=no"])?;
    if !base_dirty.is_empty() {
        return Err(tr(
            "ベースの作業ツリーに未コミットの変更があります — 綺麗にしてから採用してください",
        ));
    }
    match run_git(&race.repo, &["merge", "--no-edit", &racer.branch]) {
        Ok(out) => Ok(match parse_merge_kind(&out) {
            MergeKind::FastForward => trf(
                "✅ {branch} を {base} へ取り込みました (fast-forward)",
                &[("branch", racer.branch.clone()), ("base", race.base_branch.clone())],
            ),
            MergeKind::Merge => trf(
                "✅ {branch} を {base} へマージしました",
                &[("branch", racer.branch.clone()), ("base", race.base_branch.clone())],
            ),
            MergeKind::UpToDate => trf(
                "{base} は既に {branch} の内容を含んでいます",
                &[("base", race.base_branch.clone()), ("branch", racer.branch.clone())],
            ),
        }),
        Err(e) => {
            // マージ途中の状態 (コンフリクトマーカー等) を作業ツリーに残さない。
            let _ = run_git(&race.repo, &["merge", "--abort"]);
            Err(trf(
                "マージできませんでした (巻き戻し済み): {e}",
                &[("e", e)],
            ))
        }
    }
}

/// racer の worktree とブランチを削除する。`force` なしでは git 自身が
/// 未コミット変更のある worktree の削除を拒否する (それが安全装置)。
pub fn discard_racer(race: &Race, idx: usize, force: bool) -> Result<(), String> {
    let racer = race
        .racers
        .get(idx)
        .ok_or_else(|| tr("racer が見つかりません"))?;
    if racer.dir.exists() {
        let dir_s = racer.dir.to_string_lossy().into_owned();
        let args = worktree_remove_args(&dir_s, force);
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git(&race.repo, &args_ref)?;
    } else {
        // フォルダだけ先に消えているケース: 管理情報を掃除してからブランチを消す。
        let _ = run_git(&race.repo, &["worktree", "prune"]);
    }
    // レースブランチは未マージのまま消すのが本分なので -D (小文字 -d は拒否される)。
    run_git(&race.repo, &["branch", "-D", &racer.branch])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ダッシュボード (UI 状態 + 描画)
// ---------------------------------------------------------------------------

/// ダッシュボードから app.rs へのお願い。描画中は記録だけして、反映は app.rs が行う。
#[derive(Clone, Debug)]
pub enum RaceAction {
    /// レース開始 (プロンプト, 選択したプリセット index 列)
    Start {
        prompt: String,
        preset_indices: Vec<usize>,
    },
    /// racer のターミナルへフォーカスを移す
    Focus(usize),
    /// racer の全文差分を差分タブで開く
    OpenDiff(usize),
    /// racer のブランチをベースへマージする
    Adopt(usize),
    /// racer の worktree + ブランチを削除する
    Discard { idx: usize, force: bool },
    /// ダッシュボードを畳む (worktree とブランチはそのまま残る)
    Close,
}

/// レースダッシュボードの状態。app.rs が 1 フィールドだけ持つ。
pub struct RacePanel {
    pub race: Option<Race>,
    /// レース開始フォームの表示中フラグ
    pub form_open: bool,
    pub prompt_input: String,
    /// 選択中のプリセット index (選んだ順)
    pub selected: Vec<usize>,
    /// バッファ id → パース済み差分 (RaceDiff タブ用)
    diff_cache: HashMap<u64, Vec<FileDiff>>,
    /// 走行中の差分量収集 (racer 添字, 変更量)
    pending: Option<Receiver<Vec<(usize, DiffStat)>>>,
    last_refresh: Option<Instant>,
}

impl Default for RacePanel {
    fn default() -> Self {
        Self::new()
    }
}

impl RacePanel {
    pub fn new() -> Self {
        Self {
            race: None,
            form_open: false,
            prompt_input: String::new(),
            selected: Vec::new(),
            diff_cache: HashMap::new(),
            pending: None,
            last_refresh: None,
        }
    }

    /// レースを開始した状態にする (フォームを畳み、次フレームで差分量を取り直す)。
    pub fn begin(&mut self, race: Race) {
        self.race = Some(race);
        self.form_open = false;
        self.prompt_input.clear();
        self.selected.clear();
        self.diff_cache.clear();
        self.pending = None;
        self.last_refresh = None;
    }

    /// ダッシュボードを畳む。worktree とブランチには触らない。
    pub fn close(&mut self) {
        self.race = None;
        self.diff_cache.clear();
        self.pending = None;
        self.last_refresh = None;
    }

    /// racer のセッション id。
    pub fn session_of(&self, idx: usize) -> Option<u64> {
        self.race.as_ref()?.racers.get(idx)?.session_id
    }

    /// 差分のパース結果を捨てる (同じタブへ新しい差分を流し込んだ時)。
    pub fn drop_diff_cache(&mut self, buf_id: u64) {
        self.diff_cache.remove(&buf_id);
    }

    /// セッション一覧 (id, 走行中か) から racer の状態を追随させる。
    pub fn sync_sessions(&mut self, sessions: &[(u64, bool)]) {
        let Some(race) = &mut self.race else { return };
        for r in &mut race.racers {
            let running = r
                .session_id
                .map(|sid| sessions.iter().any(|&(id, run)| id == sid && run));
            // セッション未起動 (Preparing) のまま id が無い racer は据え置く。
            if r.session_id.is_some() {
                r.status = r.status.next(running);
            }
        }
    }

    /// racer の全文差分 (タイトル, unified diff 本文)。UI 操作の単発実行なので同期。
    pub fn full_diff(&self, idx: usize) -> Result<(String, String), String> {
        let race = self.race.as_ref().ok_or_else(|| tr("レースがありません"))?;
        let racer = race
            .racers
            .get(idx)
            .ok_or_else(|| tr("racer が見つかりません"))?;
        let text = run_git(&racer.dir, &["diff", &race.base_commit])?;
        let title = trf(
            "🏁 {name} の差分",
            &[("name", racer.preset_name.clone())],
        );
        Ok((title, text))
    }

    /// [採用]: マージに成功したら racer を Adopted にする。
    pub fn adopt(&mut self, idx: usize) -> Result<String, String> {
        let race = self.race.as_mut().ok_or_else(|| tr("レースがありません"))?;
        let msg = adopt_racer(race, idx)?;
        if let Some(r) = race.racers.get_mut(idx) {
            r.status = RacerStatus::Adopted;
            r.confirm_discard = false;
        }
        Ok(msg)
    }

    /// [破棄]: 成功したら racer を Discarded にする。失敗 (未コミット変更で git が
    /// 拒否した等) なら確認フラグを立て、次の操作だけが --force で通る。
    pub fn discard(&mut self, idx: usize, force: bool) -> Result<String, String> {
        let race = self.race.as_mut().ok_or_else(|| tr("レースがありません"))?;
        match discard_racer(race, idx, force) {
            Ok(()) => {
                if let Some(r) = race.racers.get_mut(idx) {
                    r.status = RacerStatus::Discarded;
                    r.confirm_discard = false;
                    r.stat = None;
                }
                Ok(tr("🗑 worktree とブランチを削除しました"))
            }
            Err(e) => {
                if let Some(r) = race.racers.get_mut(idx) {
                    r.confirm_discard = true;
                }
                Err(trf(
                    "{e} — もう一度 [⚠ 強制破棄] を押すと未コミットの変更ごと削除します",
                    &[("e", e)],
                ))
            }
        }
    }

    /// 飛行中の差分量収集を回収する (UI スレッドは待たない)。
    fn poll(&mut self) {
        let Some(rx) = &self.pending else { return };
        match rx.try_recv() {
            Ok(list) => {
                self.pending = None;
                if let Some(race) = &mut self.race {
                    for (i, st) in list {
                        if let Some(r) = race.racers.get_mut(i) {
                            r.stat = Some(st);
                        }
                    }
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.pending = None,
        }
    }

    /// TTL 切れなら差分量の収集を別スレッドで仕込む (git_panel と同じ流儀)。
    fn maybe_refresh(&mut self, ctx: &egui::Context) {
        let Some(race) = &self.race else { return };
        if self.pending.is_some() {
            return;
        }
        if self.last_refresh.is_some_and(|t| t.elapsed() < STAT_TTL) {
            return;
        }
        let jobs: Vec<(usize, PathBuf)> = race
            .racers
            .iter()
            .enumerate()
            .filter(|(_, r)| !matches!(r.status, RacerStatus::Discarded))
            .map(|(i, r)| (i, r.dir.clone()))
            .collect();
        if jobs.is_empty() {
            return;
        }
        let base = race.base_commit.clone();
        let (tx, rx) = channel();
        self.pending = Some(rx);
        self.last_refresh = Some(Instant::now());
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            for (i, dir) in jobs {
                if let Ok(st) = collect_stat(&dir, &base) {
                    out.push((i, st));
                }
            }
            let _ = tx.send(out);
            ctx.request_repaint();
        });
    }
}

/// racer の状態バッジ (表示文字列, 色)。
fn status_badge(s: &RacerStatus, theme: &Theme) -> (String, egui::Color32) {
    match s {
        RacerStatus::Preparing => (tr("準備中"), theme.text_dim),
        RacerStatus::Running => (tr("🏃 走行中"), theme.ok),
        RacerStatus::Exited => (tr("⏹ 終了"), theme.accent),
        RacerStatus::Adopted => (tr("✅ 採用済"), theme.ok),
        RacerStatus::Discarded => (tr("🗑 破棄済"), theme.text_dim),
        RacerStatus::Error(e) => (
            trf("⚠ {e}", &[("e", crate::notify::truncate_chars(e, 40))]),
            theme.err,
        ),
    }
}

/// Cockpit 内のレースセクション。押された操作は Vec で返し、反映は app.rs が行う。
/// `presets` は (アイコン, 名前)、`sessions` は (セッション id, 走行中か)。
pub fn race_section(
    panel: &mut RacePanel,
    ui: &mut egui::Ui,
    theme: &Theme,
    presets: &[(String, String)],
    sessions: &[(u64, bool)],
) -> Vec<RaceAction> {
    let mut acts: Vec<RaceAction> = Vec::new();
    panel.poll();
    panel.maybe_refresh(ui.ctx());
    panel.sync_sessions(sessions);

    ui.horizontal(|ui| {
        ui.label(RichText::new(tr("🏁 プロンプトレース")).strong().color(theme.accent));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if panel.race.is_some() {
                if ui
                    .button(tr("✕ レースを閉じる"))
                    .on_hover_text(tr(
                        "ダッシュボードを閉じます (worktree とブランチはそのまま残ります)",
                    ))
                    .clicked()
                {
                    acts.push(RaceAction::Close);
                }
            } else {
                let label = if panel.form_open {
                    tr("▴ フォームを畳む")
                } else {
                    tr("🏁 新しいレース…")
                };
                if ui.button(label).clicked() {
                    panel.form_open = !panel.form_open;
                }
            }
        });
    });

    if panel.race.is_none() && panel.form_open {
        race_form_ui(panel, ui, theme, presets, &mut acts);
    }
    if panel.race.is_some() {
        race_rows_ui(panel, ui, theme, &mut acts);
        // 走行中はゆっくり再描画を回し、差分量ポーリングを進める
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    }
    ui.add_space(4.0);
    acts
}

/// レース開始フォーム: プロンプト入力 + プリセット選択 (2〜4 体)。
fn race_form_ui(
    panel: &mut RacePanel,
    ui: &mut egui::Ui,
    theme: &Theme,
    presets: &[(String, String)],
    acts: &mut Vec<RaceAction>,
) {
    ui.label(
        RichText::new(tr(
            "1 つのプロンプトを複数のエージェントに同時に競わせ、良い成果だけを採用します。\
             racer ごとに独立した worktree (ブランチ race/…) が切られます。",
        ))
        .color(theme.text_dim)
        .size(11.5),
    );
    ui.add_space(4.0);
    ui.add(
        egui::TextEdit::multiline(&mut panel.prompt_input)
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .hint_text(tr("全 racer に投入するプロンプト…")),
    );
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(trf(
                "racer ({min}〜{max} 体):",
                &[("min", MIN_RACERS.to_string()), ("max", MAX_RACERS.to_string())],
            ))
            .color(theme.text_dim)
            .size(11.5),
        );
        for (i, (icon, name)) in presets.iter().enumerate() {
            let mut on = panel.selected.contains(&i);
            if ui.checkbox(&mut on, format!("{icon} {name}")).changed() {
                if on {
                    if panel.selected.len() < MAX_RACERS {
                        panel.selected.push(i);
                    }
                } else {
                    panel.selected.retain(|&x| x != i);
                }
            }
        }
    });
    let n = panel.selected.len();
    let ready = !panel.prompt_input.trim().is_empty() && (MIN_RACERS..=MAX_RACERS).contains(&n);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                ready,
                egui::Button::new(trf("🏁 スタート ({n} 体)", &[("n", n.to_string())])),
            )
            .on_hover_text(tr(
                "作業ツリーが綺麗であることが条件です (勝者をマージで戻すため)",
            ))
            .clicked()
        {
            acts.push(RaceAction::Start {
                prompt: panel.prompt_input.trim().to_string(),
                preset_indices: panel.selected.clone(),
            });
        }
    });
}

/// racer の一覧行: アイコン+名前 / ブランチ / 状態 / 変更量 / 操作ボタン。
fn race_rows_ui(
    panel: &mut RacePanel,
    ui: &mut egui::Ui,
    theme: &Theme,
    acts: &mut Vec<RaceAction>,
) {
    let Some(race) = &panel.race else { return };
    ui.label(
        RichText::new(trf(
            "ベース: {base} — {prompt}",
            &[
                ("base", race.base_branch.clone()),
                ("prompt", crate::notify::truncate_chars(&race.prompt, 60)),
            ],
        ))
        .color(theme.text_dim)
        .size(11.0),
    );
    for (i, r) in race.racers.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} {}", r.icon, r.preset_name)).strong());
            ui.label(
                RichText::new(&r.branch)
                    .monospace()
                    .color(theme.text_dim)
                    .size(10.5),
            );
            let (badge, color) = status_badge(&r.status, theme);
            ui.label(RichText::new(badge).color(color).size(11.0));
            let stat_text = match &r.stat {
                Some(st) => {
                    let mut t = trf(
                        "{f} ファイル +{a} -{d}",
                        &[
                            ("f", st.files.to_string()),
                            ("a", st.insertions.to_string()),
                            ("d", st.deletions.to_string()),
                        ],
                    );
                    if st.untracked > 0 {
                        t.push_str(&trf(" (未追跡 {u})", &[("u", st.untracked.to_string())]));
                    }
                    t
                }
                None => "…".to_string(),
            };
            ui.label(RichText::new(stat_text).color(theme.text_dim).size(11.0));

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let discarded = matches!(r.status, RacerStatus::Discarded);
                // [破棄] — confirm_discard 済みなら強制版に変わる
                if !discarded {
                    if r.confirm_discard {
                        if ui
                            .button(RichText::new(tr("⚠ 強制破棄")).color(theme.err))
                            .on_hover_text(tr("未コミットの変更ごと worktree とブランチを削除します"))
                            .clicked()
                        {
                            acts.push(RaceAction::Discard { idx: i, force: true });
                        }
                    } else if ui
                        .button(tr("破棄"))
                        .on_hover_text(tr(
                            "この racer の worktree とブランチを削除します\n(未コミットの変更があれば拒否されます)",
                        ))
                        .clicked()
                    {
                        acts.push(RaceAction::Discard { idx: i, force: false });
                    }
                }
                // [採用] — マージ済み/破棄済みには出さない
                if !r.status.settled() {
                    if ui
                        .button(RichText::new(tr("採用")).color(theme.ok))
                        .on_hover_text(trf(
                            "{branch} を {base} へマージします (コミット済みの成果だけ)",
                            &[("branch", r.branch.clone()), ("base", race.base_branch.clone())],
                        ))
                        .clicked()
                    {
                        acts.push(RaceAction::Adopt(i));
                    }
                }
                if !discarded && ui.button(tr("Diff")).on_hover_text(tr("ベースとの全文差分をタブで開く")).clicked() {
                    acts.push(RaceAction::OpenDiff(i));
                }
                if r.session_id.is_some()
                    && ui.button(tr("表示")).on_hover_text(tr("この racer のターミナルを表示")).clicked()
                {
                    acts.push(RaceAction::Focus(i));
                }
            });
        });
    }
    if race.racers.iter().all(|r| r.status.settled()) {
        ui.label(
            RichText::new(tr("全 racer が決着しました — [✕ レースを閉じる] で片付けられます"))
                .color(theme.text_dim)
                .size(11.0),
        );
    }
}

/// レース差分タブの中身。読み取り専用 (panels.rs の PR 差分タブと同じ流儀で、
/// パース結果はバッファ id をキーにキャッシュする)。
pub fn race_diff_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    slot: usize,
    buf_id: u64,
    text: &str,
    panel: &mut RacePanel,
) {
    let who = panel
        .race
        .as_ref()
        .and_then(|r| r.racers.get(slot))
        .map(|r| format!("{} {} ({})", r.icon, r.preset_name, r.branch))
        .unwrap_or_else(|| tr("レース差分"));
    if panel.diff_cache.len() > DIFF_CACHE_CAP {
        panel.diff_cache.clear();
    }
    let files = panel
        .diff_cache
        .entry(buf_id)
        .or_insert_with(|| diff::parse_unified(text));

    let (add, del): (u64, u64) = files.iter().fold((0, 0), |(a, d), f| {
        (a + f.additions as u64, d + f.deletions as u64)
    });
    egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(10.0, 6.0))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(trf("🏁 {who} の差分", &[("who", who)])).strong());
                ui.label(
                    RichText::new(trf(
                        "{n} ファイル · +{add} -{del} · 読み取り専用",
                        &[
                            ("n", files.len().to_string()),
                            ("add", add.to_string()),
                            ("del", del.to_string()),
                        ],
                    ))
                    .color(theme.text_dim)
                    .size(11.0),
                );
            });
        });

    egui::ScrollArea::vertical()
        .id_salt(("zv-race-diff", buf_id))
        .auto_shrink(false)
        .show(ui, |ui| {
            if files.is_empty() {
                ui.add_space(8.0);
                ui.label(
                    RichText::new(tr("差分はありません (未追跡ファイルはここに出ません)"))
                        .color(theme.text_dim)
                        .size(11.5),
                );
            } else {
                diff::diff_ui(ui, theme, files);
            }
        });
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── スラグ / ブランチ名 ────────────────────────────────────────

    #[test]
    fn slug_from_english_prompt() {
        assert_eq!(slugify_prompt("Add dark mode toggle"), "add-dark-mode-toggle");
        assert_eq!(slugify_prompt("  Fix   bug #42!  "), "fix-bug-42");
    }

    #[test]
    fn slug_from_japanese_prompt_falls_back() {
        // ASCII が 1 文字も残らない → "race"
        assert_eq!(slugify_prompt("ダークモードを追加して"), "race");
        assert_eq!(slugify_prompt(""), "race");
        assert_eq!(slugify_prompt("---!!!---"), "race");
    }

    #[test]
    fn slug_mixed_and_truncated() {
        // 日本語混じりでも ASCII 部分は残る
        assert_eq!(slugify_prompt("APIのrate limitを直す"), "api-rate-limit");
        // 長いプロンプトは 24 文字で打ち切り、末尾のダッシュは残さない
        let s = slugify_prompt("aaaa bbbb cccc dddd eeee ffff gggg");
        assert!(s.len() <= SLUG_MAX, "len={} s={s}", s.len());
        assert!(!s.ends_with('-'));
    }

    #[test]
    fn branch_and_dir_names() {
        assert_eq!(race_branch("fix-bug", 2), "race/fix-bug-2");
        assert_eq!(worktree_dir_name("myrepo", "fix-bug", 3), "myrepo-race-fix-bug-3");
    }

    #[test]
    fn unique_slug_no_collision_keeps_base() {
        assert_eq!(unique_slug("fix", |_| false), "fix");
    }

    #[test]
    fn unique_slug_bumps_until_free() {
        // "fix" と "fix-r2" が使用中 → "fix-r3"
        let used = ["fix", "fix-r2"];
        let got = unique_slug("fix", |c| used.contains(&c));
        assert_eq!(got, "fix-r3");
    }

    // ── worktree の置き場 ─────────────────────────────────────────

    #[test]
    fn worktree_base_is_repo_parent_normally() {
        let repo = std::env::temp_dir().join("some").join("repo");
        assert_eq!(race_worktree_base(&repo), std::env::temp_dir().join("some"));
    }

    #[test]
    fn worktree_base_avoids_claude_worktrees() {
        // Claude セッションの管理領域 (.claude/worktrees) は汚さない
        let repo = std::env::temp_dir()
            .join("proj")
            .join(".claude")
            .join("worktrees")
            .join("night-x");
        assert_eq!(
            race_worktree_base(&repo),
            std::env::temp_dir().join("zaivern-races")
        );
    }

    // ── コマンドライン組み立て ─────────────────────────────────────

    #[test]
    fn worktree_remove_args_force_flag() {
        assert_eq!(
            worktree_remove_args("/x/wt", false),
            vec!["worktree", "remove", "/x/wt"]
        );
        assert_eq!(
            worktree_remove_args("/x/wt", true),
            vec!["worktree", "remove", "--force", "/x/wt"]
        );
    }

    // ── shortstat / merge 出力のパース ─────────────────────────────

    #[test]
    fn shortstat_full_line() {
        let st = parse_shortstat(" 3 files changed, 10 insertions(+), 2 deletions(-)");
        assert_eq!(
            st,
            DiffStat { files: 3, insertions: 10, deletions: 2, untracked: 0 }
        );
    }

    #[test]
    fn shortstat_singular_and_partial() {
        let st = parse_shortstat(" 1 file changed, 1 insertion(+)");
        assert_eq!(st.files, 1);
        assert_eq!(st.insertions, 1);
        assert_eq!(st.deletions, 0);
    }

    #[test]
    fn shortstat_empty_is_zero() {
        assert_eq!(parse_shortstat(""), DiffStat::default());
    }

    #[test]
    fn merge_kind_parsing() {
        assert_eq!(
            parse_merge_kind("Updating 1234..5678\nFast-forward\n a.txt | 1 +"),
            MergeKind::FastForward
        );
        assert_eq!(
            parse_merge_kind("Merge made by the 'ort' strategy."),
            MergeKind::Merge
        );
        assert_eq!(parse_merge_kind("Already up to date."), MergeKind::UpToDate);
    }

    // ── 状態遷移 ──────────────────────────────────────────────────

    #[test]
    fn status_transitions() {
        // 起動 → 走行 → 終了
        assert_eq!(RacerStatus::Preparing.next(Some(true)), RacerStatus::Running);
        assert_eq!(RacerStatus::Running.next(Some(false)), RacerStatus::Exited);
        // セッションが消えた (タブごと閉じた) 場合も終了扱い
        assert_eq!(RacerStatus::Running.next(None), RacerStatus::Exited);
        // 終端状態は覆らない
        assert_eq!(RacerStatus::Adopted.next(Some(true)), RacerStatus::Adopted);
        assert_eq!(RacerStatus::Discarded.next(Some(true)), RacerStatus::Discarded);
        let err = RacerStatus::Error("x".into());
        assert_eq!(err.next(Some(true)), err);
    }

    #[test]
    fn sync_sessions_updates_only_launched_racers() {
        let mut panel = RacePanel::new();
        panel.race = Some(Race {
            repo: PathBuf::from("."),
            base_branch: "main".into(),
            base_commit: "deadbeef".into(),
            prompt: "p".into(),
            racers: vec![
                Racer {
                    preset_name: "A".into(),
                    icon: "👾".into(),
                    branch: "race/x-1".into(),
                    dir: PathBuf::from("a"),
                    session_id: Some(7),
                    status: RacerStatus::Running,
                    stat: None,
                    confirm_discard: false,
                },
                Racer {
                    preset_name: "B".into(),
                    icon: "⚡".into(),
                    branch: "race/x-2".into(),
                    dir: PathBuf::from("b"),
                    session_id: None,
                    status: RacerStatus::Preparing,
                    stat: None,
                    confirm_discard: false,
                },
            ],
        });
        // id 7 は終了、もう 1 体は未起動のまま
        panel.sync_sessions(&[(7, false)]);
        let race = panel.race.as_ref().unwrap();
        assert_eq!(race.racers[0].status, RacerStatus::Exited);
        assert_eq!(race.racers[1].status, RacerStatus::Preparing, "未起動の racer は据え置き");
    }

    #[test]
    fn race_prompt_carries_prompt_and_branch() {
        let p = build_race_prompt("ダークモード追加", "race/dark-1");
        assert!(p.contains("ダークモード追加"));
        assert!(p.contains("race/dark-1"));
        assert!(p.contains("コミット"), "コミットの約束事が入る");
    }

    // ── git 実フィクスチャ (git が無い環境ではスキップ) ─────────────

    /// 初期コミット済みの使い捨てリポジトリ。git が無ければ None。
    fn fixture_repo(tag: &str) -> Option<PathBuf> {
        let dir = crate::test_util::unique_temp_dir("zaivern-race", tag);
        if run_git(&dir, &["init", "--quiet"]).is_err() {
            std::fs::remove_dir_all(&dir).ok();
            return None; // git が無い環境ではスキップ
        }
        std::fs::write(dir.join("a.txt"), "base\n").expect("write a.txt");
        git_ok(&dir, &["add", "."]);
        commit(&dir, "init");
        Some(dir)
    }

    /// 署名者を -c で毎回与えてコミットする (CI に user.name が無くても通る)。
    fn commit(repo: &Path, msg: &str) {
        git_ok(
            repo,
            &[
                "-c",
                "user.name=zaivern-test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                msg,
            ],
        );
    }

    fn git_ok(repo: &Path, args: &[&str]) {
        run_git(repo, args).unwrap_or_else(|e| panic!("git {args:?} 失敗: {e}"));
    }

    /// レース + 作った worktree を後片付けする。
    fn cleanup(race: &Race) {
        for r in &race.racers {
            std::fs::remove_dir_all(&r.dir).ok();
        }
        std::fs::remove_dir_all(&race.repo).ok();
    }

    fn two_presets() -> Vec<(String, String)> {
        vec![
            ("👾".to_string(), "Claude".to_string()),
            ("⚡".to_string(), "Codex".to_string()),
        ]
    }

    #[test]
    fn start_race_creates_worktrees_and_branches() {
        let Some(repo) = fixture_repo("start") else { return };
        let race = start_race(&repo, "add dark mode", &two_presets()).expect("開始できる");
        assert_eq!(race.racers.len(), 2);
        assert_eq!(race.racers[0].branch, "race/add-dark-mode-1");
        assert_eq!(race.racers[1].branch, "race/add-dark-mode-2");
        for r in &race.racers {
            assert!(r.dir.is_dir(), "worktree が実在する: {}", r.dir.display());
            assert!(!r.dir.starts_with(&race.repo), "リポジトリの外に切られる");
            // worktree の HEAD は自分のブランチ
            let head = run_git(&r.dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
            assert_eq!(head, r.branch);
        }
        let branches = run_git(&race.repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads"]).unwrap();
        assert!(branches.contains("race/add-dark-mode-1"));
        assert!(branches.contains("race/add-dark-mode-2"));

        // 同じプロンプトで 2 本目 → スラグが -r2 に繰り上がる (一意性の端到端)
        let race2 = start_race(&repo, "add dark mode", &two_presets()).expect("2 本目");
        assert_eq!(race2.racers[0].branch, "race/add-dark-mode-r2-1");
        cleanup(&race2);
        cleanup(&race);
    }

    #[test]
    fn start_race_refuses_dirty_tree() {
        let Some(repo) = fixture_repo("dirty") else { return };
        std::fs::write(repo.join("a.txt"), "modified\n").expect("dirty write");
        let err = start_race(&repo, "x y", &two_presets()).expect_err("汚れたツリーでは開始しない");
        assert!(err.contains("未コミット"), "err={err}");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn adopt_fast_forwards_committed_work() {
        let Some(repo) = fixture_repo("adopt") else { return };
        let race = start_race(&repo, "feature x", &two_presets()).expect("開始");
        // racer 1 が成果をコミットする
        let r0 = &race.racers[0];
        std::fs::write(r0.dir.join("win.txt"), "winner\n").expect("write");
        git_ok(&r0.dir, &["add", "."]);
        commit(&r0.dir, "winner work");

        let msg = adopt_racer(&race, 0).expect("採用できる");
        assert!(msg.contains("fast-forward"), "msg={msg}");
        assert!(race.repo.join("win.txt").is_file(), "ベースへ取り込まれる");

        // 敗者は破棄できる (綺麗な worktree なので force 不要)
        discard_racer(&race, 1, false).expect("敗者の破棄");
        assert!(!race.racers[1].dir.exists());
        let branches = run_git(&race.repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads"]).unwrap();
        assert!(!branches.contains(&race.racers[1].branch), "ブランチも消える");
        cleanup(&race);
    }

    #[test]
    fn adopt_refuses_uncommitted_racer_changes() {
        let Some(repo) = fixture_repo("adopt-dirty") else { return };
        let race = start_race(&repo, "feature y", &two_presets()).expect("開始");
        std::fs::write(race.racers[0].dir.join("a.txt"), "uncommitted\n").expect("write");
        let err = adopt_racer(&race, 0).expect_err("未コミットの成果は採用しない");
        assert!(err.contains("コミット"), "err={err}");
        cleanup(&race);
    }

    #[test]
    fn adopt_conflict_aborts_and_leaves_base_clean() {
        let Some(repo) = fixture_repo("conflict") else { return };
        let race = start_race(&repo, "conflict z", &two_presets()).expect("開始");
        // racer とベースが同じファイルの同じ行を別内容に変える
        std::fs::write(race.racers[0].dir.join("a.txt"), "racer version\n").expect("write");
        git_ok(&race.racers[0].dir, &["add", "."]);
        commit(&race.racers[0].dir, "racer change");
        std::fs::write(repo.join("a.txt"), "base version\n").expect("write");
        git_ok(&repo, &["add", "."]);
        commit(&repo, "base change");

        let err = adopt_racer(&race, 0).expect_err("コンフリクトは失敗する");
        assert!(err.contains("マージできません"), "err={err}");
        // --abort 済み: ベースは綺麗で、内容もベース側のまま
        let status = run_git(&race.repo, &["status", "--porcelain"]).unwrap();
        assert!(status.is_empty(), "巻き戻し後は綺麗: {status}");
        let body = std::fs::read_to_string(repo.join("a.txt")).unwrap();
        assert_eq!(body, "base version\n");
        cleanup(&race);
    }

    #[test]
    fn discard_dirty_needs_force() {
        let Some(repo) = fixture_repo("discard") else { return };
        let race = start_race(&repo, "discard w", &two_presets()).expect("開始");
        // 未コミットの変更がある worktree は force 無しでは消えない
        std::fs::write(race.racers[0].dir.join("junk.txt"), "junk\n").expect("write");
        let err = discard_racer(&race, 0, false).expect_err("汚れた worktree は拒否");
        assert!(err.contains("git worktree"), "err={err}");
        assert!(race.racers[0].dir.exists(), "worktree は残っている");
        // force なら消える
        discard_racer(&race, 0, true).expect("強制破棄");
        assert!(!race.racers[0].dir.exists());
        cleanup(&race);
    }

    #[test]
    fn panel_discard_sets_confirm_flag_on_refusal() {
        let Some(repo) = fixture_repo("panel-discard") else { return };
        let race = start_race(&repo, "panel v", &two_presets()).expect("開始");
        let mut panel = RacePanel::new();
        panel.begin(race);
        // 汚す → force 無しの破棄は失敗し、確認フラグが立つ
        {
            let race = panel.race.as_ref().unwrap();
            std::fs::write(race.racers[0].dir.join("junk.txt"), "junk\n").expect("write");
        }
        let err = panel.discard(0, false).expect_err("拒否される");
        assert!(err.contains("強制破棄"), "誘導文が付く: {err}");
        assert!(panel.race.as_ref().unwrap().racers[0].confirm_discard);
        // 2 度目 (force) は通り、状態が Discarded になる
        panel.discard(0, true).expect("強制破棄");
        let race = panel.race.as_ref().unwrap();
        assert_eq!(race.racers[0].status, RacerStatus::Discarded);
        assert!(!race.racers[0].confirm_discard);
        let snapshot = race.clone();
        cleanup(&snapshot);
    }
}
