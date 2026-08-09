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
//! ## 衝突の事前検出 (このモジュールの差別化点)
//!
//! N 体を並走させると「どれも正しいが互いにぶつかる差分」が量産され、
//! 並列で稼いだ時間をレビューとマージで払い戻すことになる。よくある実装は
//! **レビュー時に初めて衝突に気付く**。ここでは**走っている最中に気付く**:
//!
//! - 差分量ポーリングと同じ 1 スレッド・同じ TTL で、各 racer が触ったファイル集合を
//!   集める (未コミット = `status --porcelain=v1 -z` / コミット済み =
//!   `diff --name-only -z <base>...HEAD`)。git は UI スレッドでは一切叩かない。
//! - 集合の重なりを [`compute_overlaps`] で畳み、行ごとの ⚠ バッジと
//!   「N ファイルが 2 体以上で競合」のサマリとして即座に見せる。単独なら緑。
//! - [採用] は 2 体目が本番。既に採用済みの racer とファイルが重なるときだけ
//!   [`adopt_decision`] が確認を要求し、[破棄] と同じ 2 段クリックにする
//!   (**止めはしない — 知らせるだけ**)。
//! - 出走時に担当範囲を配って衝突自体を減らす道も [`build_scoped_race_prompt`] で
//!   用意してある (プロンプトに 1 行足すだけの純粋関数)。
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

use std::collections::{BTreeMap, HashMap, HashSet};
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

/// 差分量ポーリングの間隔。
const STAT_TTL: Duration = Duration::from_secs(4);

/// パース済み差分キャッシュの上限 (panels.rs の PR 差分タブと同じ流儀)。
const DIFF_CACHE_CAP: usize = 16;

/// 衝突バッジに直接並べるファイル名の本数 (残りは「他 N 件」に畳む)。
const CONFLICT_INLINE_MAX: usize = 2;

/// 衝突バッジ / ツールチップで 1 パスに許す文字数。
const PATH_CHARS_MAX: usize = 44;

/// [🏆 勝者を評価] を文字つきで置くのに要る残り幅。これを切ったらアイコンだけにする。
const EVAL_BTN_FULL_W: f32 = 130.0;

// ---------------------------------------------------------------------------
// 純粋ロジック: スラグ / ブランチ名 / パース
// ---------------------------------------------------------------------------

/// プロンプトからブランチ用スラグを作る。ASCII 英数字だけを残し、他は `-` に
/// 潰して連結する。日本語だけのプロンプトのように何も残らない場合は "race"。
pub fn slugify_prompt(prompt: &str) -> String {
    crate::worktree::slugify(prompt, "race")
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
    crate::worktree::worktree_base(repo, "zaivern-races")
}

/// `git worktree remove` の引数列。--force は UI の確認フラグを経た時だけ付く。
/// 実体は [`crate::worktree`] (通常運用の隔離 worktree と同じ道を通す)。
pub use crate::worktree::worktree_remove_args;

/// racer に自動投入するプロンプト本文。ユーザーの指示 + レースの約束事。
pub fn build_race_prompt(prompt: &str, branch: &str) -> String {
    format!(
        "{prompt}\n\n(これは複数エージェントの並走レースです。この作業ツリーはあなた専用の\
         ブランチ {branch} です。実装が終わったらテストを通してコミットしてください。)"
    )
}

/// 担当範囲ヒント付きの投入プロンプト。ディレクトリや glob を racer ごとに配ると、
/// エージェントが自然に住み分けて衝突そのものが起きにくくなる (出走時の分割)。
/// ヒントが空 / 空白だけなら [`build_race_prompt`] と完全に同じ文面を返す
/// (「ヒント無しなら 1 文字も変わらない」— 既存の投入経路を壊さないための約束)。
///
/// 現時点では UI からヒントを配る導線がまだ無い (フォームに列を足すのは別の波)。
/// 文面の仕様だけ先にここで固めてテストで縛っておき、配線はあとから足す。
#[allow(dead_code)] // UI 導線が付くまでの間だけ
pub fn build_scoped_race_prompt(prompt: &str, branch: &str, scope: Option<&str>) -> String {
    let base = build_race_prompt(prompt, branch);
    match scope.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => format!(
            "{base}\n\n(担当範囲: {s} — 他のレーサーと同じファイルを奪い合わないよう、\
             変更はできるだけこの範囲に閉じてください。範囲外に触る必要が出たら、\
             最小限に留めて理由を書き残してください。)"
        ),
        None => base,
    }
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
// 純粋ロジック: 触ったファイルの収集と衝突検出
// ---------------------------------------------------------------------------

// `git status --porcelain=v1 -z` / `diff --name-only -z` のパーサは
// [`crate::worktree`] が持つ (通常運用のファイル衝突検出と同じ実装を使う)。
// ここでは回帰テストだけが直接呼ぶので、取り込みは `mod tests` の中で行う。

// 触ったファイルの重なりを畳む部分は [`crate::worktree`] が持つ。
// レースも通常運用の Cockpit も**同じ 1 実装**を通る (添字は
// [`Race::racers`] の添字として使う)。
pub use crate::worktree::{compute_overlaps, OverlapReport};

/// [採用] を押したときの判断。**塞がずに知らせる**のが方針で、`NeedsConfirm` は
/// 「1 度目のクリックは警告に使う」という意味 (破棄の 2 段確認と同じ流儀)。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AdoptDecision {
    /// 採用済みが居ない、または触ったファイルが重ならない → そのまま採用してよい。
    Proceed,
    /// 採用済みの racer とファイルが重なる & 未確認 → 警告して 2 度目を待つ。
    NeedsConfirm {
        /// 重なった相手 (racer 添字, 昇順)
        with: Vec<usize>,
        /// 重なったファイル (昇順)
        files: Vec<PathBuf>,
    },
    /// 重なるが確認済み (2 度目のクリック) → 承知の上で採用を通す。
    ConfirmedProceed {
        with: Vec<usize>,
        files: Vec<PathBuf>,
    },
}

impl AdoptDecision {
    /// 実際にマージへ進んでよいか。
    pub fn may_merge(&self) -> bool {
        !matches!(self, AdoptDecision::NeedsConfirm { .. })
    }

    /// 重なった相手 (racer 添字)。重なりなしなら空。
    pub fn conflict_with(&self) -> &[usize] {
        match self {
            AdoptDecision::Proceed => &[],
            AdoptDecision::NeedsConfirm { with, .. }
            | AdoptDecision::ConfirmedProceed { with, .. } => with,
        }
    }

    /// 重なったファイル。重なりなしなら空。
    pub fn conflict_files(&self) -> &[PathBuf] {
        match self {
            AdoptDecision::Proceed => &[],
            AdoptDecision::NeedsConfirm { files, .. }
            | AdoptDecision::ConfirmedProceed { files, .. } => files,
        }
    }
}

/// 2 体目以降の採用で本当にぶつかるかを判定する。
///
/// `adopted` は既にベースへマージ済みの racer とその触ったファイル、
/// `candidate` はこれから採用しようとしている racer の触ったファイル。
/// `confirmed` は「1 度警告を見た上でもう一度押した」フラグ。
pub fn adopt_decision(
    adopted: &[(usize, HashSet<PathBuf>)],
    candidate: &HashSet<PathBuf>,
    confirmed: bool,
) -> AdoptDecision {
    let mut with: Vec<usize> = Vec::new();
    let mut files: BTreeMap<PathBuf, ()> = BTreeMap::new();
    for (idx, set) in adopted {
        let mut hit = false;
        for f in set.intersection(candidate) {
            files.insert(f.clone(), ());
            hit = true;
        }
        if hit {
            with.push(*idx);
        }
    }
    if with.is_empty() {
        return AdoptDecision::Proceed;
    }
    with.sort_unstable();
    let files: Vec<PathBuf> = files.into_keys().collect();
    if confirmed {
        AdoptDecision::ConfirmedProceed { with, files }
    } else {
        AdoptDecision::NeedsConfirm { with, files }
    }
}

/// ファイル列を「a.rs, b.rs 他 2 件」の形に畳む (バッジ用の短い表記)。
pub fn summarize_files(files: &[PathBuf], max: usize) -> String {
    if files.is_empty() {
        return String::new();
    }
    let shown: Vec<String> = files
        .iter()
        .take(max.max(1))
        .map(|p| crate::notify::truncate_chars(&p.to_string_lossy(), PATH_CHARS_MAX))
        .collect();
    let mut s = shown.join(", ");
    let rest = files.len().saturating_sub(shown.len());
    if rest > 0 {
        s.push_str(&trf(" 他 {n} 件", &[("n", rest.to_string())]));
    }
    s
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
    /// この racer が触ったファイル (リポジトリ相対)。差分量と同じポーリングで
    /// 更新される。None は「まだ 1 度も集めていない」。
    pub touched: Option<HashSet<PathBuf>>,
    /// [破棄] の 2 段確認。未コミット変更で git が削除を拒否した後に立ち、
    /// 次の [⚠ 強制破棄] だけが --force を付ける。
    pub confirm_discard: bool,
    /// [採用] の 2 段確認。採用済みの racer とファイルが重なるときだけ立ち、
    /// 次の [⚠ それでも採用] だけがマージへ進む。
    pub confirm_adopt: bool,
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
    crate::worktree::git_out(repo, args)
}

/// レースを開始する: リポジトリ検証 → 汚れチェック → racer ごとの worktree 作成。
/// `presets` は (アイコン, プリセット名) — panels.rs の Issue 着手メニューと同じ形。
/// エージェントの起動は呼び出し側 (app.rs) が行う。
pub fn start_race(root: &Path, prompt: &str, presets: &[(String, String)]) -> Result<Race, String> {
    let n = presets.len();
    if !(MIN_RACERS..=MAX_RACERS).contains(&n) {
        return Err(trf(
            "レースは {min}〜{max} 体で行います",
            &[
                ("min", MIN_RACERS.to_string()),
                ("max", MAX_RACERS.to_string()),
            ],
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
                || base_dir
                    .join(worktree_dir_name(&repo_name, cand, i))
                    .exists()
        })
    });
    std::fs::create_dir_all(&base_dir).map_err(|e| {
        trf(
            "worktree の置き場を作れません: {e}",
            &[("e", e.to_string())],
        )
    })?;

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
            touched: None,
            confirm_discard: false,
            confirm_adopt: false,
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

/// racer の変更量と「触ったファイル集合」を **1 回のポーリングでまとめて** 集める。
///
/// - 変更量: `git diff --shortstat <base>` は「作業ツリー vs base」なので
///   コミット済み + 未コミットの両方が載る。未追跡ファイル数は status から別集計。
/// - 触ったファイル: `status --porcelain=v1 -z` (未コミット + 未追跡) と
///   `diff --name-only -z <base>...HEAD` (コミット済み) の合併。`...` にするのは、
///   racer がベースの更新を取り込んでいてもマージベース以降だけを見るため。
///
/// 呼ばれるのは必ずワーカースレッド側 (UI スレッドで git を待たない)。
fn collect_scan(dir: &Path, base_commit: &str) -> Result<(DiffStat, HashSet<PathBuf>), String> {
    let short = run_git(dir, &["diff", "--shortstat", base_commit])?;
    let mut st = parse_shortstat(&short);
    st.untracked = crate::worktree::status_entries(dir)?
        .iter()
        .filter(|e| e.is_untracked())
        .count();
    // 未コミット + 未追跡 + コミット済み (`<base>...HEAD`) の合併。
    let touched = crate::worktree::scan_touched(dir, Some(base_commit))?;
    Ok((st, touched))
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
    let base_dirty = run_git(
        &race.repo,
        &["status", "--porcelain", "--untracked-files=no"],
    )?;
    if !base_dirty.is_empty() {
        return Err(tr(
            "ベースの作業ツリーに未コミットの変更があります — 綺麗にしてから採用してください",
        ));
    }
    match run_git(&race.repo, &["merge", "--no-edit", &racer.branch]) {
        Ok(out) => Ok(match parse_merge_kind(&out) {
            MergeKind::FastForward => trf(
                "✅ {branch} を {base} へ取り込みました (fast-forward)",
                &[
                    ("branch", racer.branch.clone()),
                    ("base", race.base_branch.clone()),
                ],
            ),
            MergeKind::Merge => trf(
                "✅ {branch} を {base} へマージしました",
                &[
                    ("branch", racer.branch.clone()),
                    ("base", race.base_branch.clone()),
                ],
            ),
            MergeKind::UpToDate => trf(
                "{base} は既に {branch} の内容を含んでいます",
                &[
                    ("base", race.base_branch.clone()),
                    ("branch", racer.branch.clone()),
                ],
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
// 勝者評価 (Crown evaluation) — **すべて in-process**
// ---------------------------------------------------------------------------
//
// N 体を走らせた後に残る最後の人力が「どれが勝ったか」の判定である。ここはそれを
// 助ける層で、次の 3 点を守る:
//
// 1. **提案であって決定ではない。** 出すのは `{勝者, 理由}` だけで、採用 (マージ) は
//    従来どおりユーザーの明示操作。評価結果で勝手にマージすることは絶対にしない。
// 2. **明示操作でだけ走る。** ボタン / パレットから 1 回。走行中の自動評価はしない
//    (設計原則 3: アイドル時のコストはゼロ)。git を叩くのでワーカースレッド。
// 3. **壊れても race は壊れない。** 収集も判定も失敗は `EvalOutcome::Failed` に畳み、
//    採用・破棄・差分表示は一切影響を受けない。評価は付加価値であって前提ではない。
//
// 「in-process」は `supervisor` の Diagnostician と同じ原則である。判定は外部 CLI
// エージェントへ投げず、この場で決定的に決める。将来 LLM を判定役に差すときのために
// **依頼 (`build_eval_request`) と応答の解析 (`parse_verdict`) を判定本体から分けて**
// あるので、`judge_in_process` を差し替えるだけで経路は変わらない。

/// 評価に必要な候補の最小本数。これ未満なら比べる意味がないのでスキップする。
pub const MIN_EVAL_CANDIDATES: usize = 2;

/// 理由として採用する最大文字数 (diagnostician の `MAX_WHY_CHARS` と同じ流儀)。
pub const MAX_REASON_CHARS: usize = 300;

/// 除外サマリに名前を並べるファイル数 (残りは「他 N 件」に畳む)。
const EXCLUDE_INLINE_MAX: usize = 3;

/// 評価に載せる候補 1 本。
///
/// `Serialize` を持つのは飾りではない: 判定役への入力は**依頼 JSON だけ**という
/// 形を守るため、`build_eval_request` がこれをそのまま書き出し、
/// `judge_in_process` はその JSON を読んで判定する (外部判定役と同じ入口)。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Candidate {
    /// `Race::racers` の添字 (勝者を racer へ戻すため)
    pub idx: usize,
    /// 表示名 = 判定の「勝者名」。同名プリセットが並んでも一意になるよう作る。
    pub name: String,
    pub branch: String,
    /// 除外・切り詰めを済ませた unified diff
    pub diff: String,
    /// 差分に残ったファイル数
    pub files: usize,
    pub insertions: usize,
    pub deletions: usize,
    /// 差分に残ったファイルのパス (テスト有無などの判定に使う)
    pub paths: Vec<String>,
    /// 落としたパス (ロックファイル / ビルド成果物 / バイナリ / 巨大生成物)
    pub excluded: Vec<String>,
    /// 上限で切り詰めたか。**黙って切らない**ので本文にも印が入る。
    pub truncated: bool,
    /// 未コミット (未追跡を含む) の変更を抱えているか。
    /// [採用] はコミット済みの成果しかマージできないので、これは重い減点になる。
    pub uncommitted: bool,
    /// 他の racer と取り合っているファイル数
    pub conflicts: usize,
}

impl Candidate {
    /// 差分が 1 バイトも無い (= まだ何も出していない)。
    pub fn is_empty(&self) -> bool {
        self.diff.trim().is_empty()
    }

    /// 変更行の合計 (規模の比較に使う)。
    pub fn churn(&self) -> usize {
        self.insertions + self.deletions
    }
}

/// 評価 1 回ぶんの結果。**Decided は提案** — 採用はユーザーが選ぶ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvalOutcome {
    /// 比べる意味がないので評価しなかった (理由つき)。
    Skipped(String),
    /// 勝者と理由。`note` は除外・切り詰めの内訳 (根拠の透明性のため必ず作る)。
    Decided {
        winner: usize,
        name: String,
        reason: String,
        note: String,
    },
    /// 評価そのものが失敗した。採用フローには影響しない。
    Failed(String),
}

/// 判定役の応答 (`{"winner", "reason"}`) を解析した結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub winner: String,
    pub reason: String,
}

/// unified diff を 1 ファイルぶんに切ったもの。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffChunk {
    pub path: String,
    pub body: String,
    /// `Binary files ... differ` / `GIT binary patch` を含む = 読ませても意味がない
    pub binary: bool,
    /// 最長行のバイト数 (minified な生成物の検出に使う)
    pub max_line: usize,
}

// ── 純粋ロジック: 差分の切り分け / 除外 / 切り詰め ─────────────────

/// `a/` `b/` の接頭辞を落とす (git の既定プレフィックス)。
fn strip_ab(p: &str) -> &str {
    p.strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p)
}

/// 1 ファイルぶんのヘッダ領域からパスを取る。
///
/// `+++ b/<path>` / `--- a/<path>` を優先する — **行末までがパス**なので、空白を
/// 含むファイル名でも壊れない。ハンク (`@@`) に入った後の `+++` は本文なので見ない。
/// どちらも無い (モード変更だけ等) ときだけ `diff --git` 行から推測する。
fn chunk_path(header: &str, body: &str) -> String {
    let mut plus: Option<&str> = None;
    let mut minus: Option<&str> = None;
    for line in body.lines() {
        if line.starts_with("@@ ") {
            break;
        }
        if let Some(p) = line.strip_prefix("+++ ") {
            if p != "/dev/null" && plus.is_none() {
                plus = Some(strip_ab(p));
            }
        } else if let Some(p) = line.strip_prefix("--- ") {
            if p != "/dev/null" && minus.is_none() {
                minus = Some(strip_ab(p));
            }
        }
    }
    if let Some(p) = plus.or(minus) {
        return p.to_string();
    }
    // 最後の手段: `diff --git a/x b/x` の a/ 側。
    let rest = header.trim_start_matches("diff --git ").trim();
    if let Some(a) = rest.strip_prefix("a/") {
        if let Some(cut) = a.find(" b/") {
            return a[..cut].to_string();
        }
        return a.to_string();
    }
    rest.to_string()
}

/// unified diff (`git diff`) を `diff --git` 行ごとに切る。
///
/// git が出す形だけを見る純関数なので、テストは git 無しで書ける。
pub fn split_diff_files(text: &str) -> Vec<DiffChunk> {
    let mut out: Vec<DiffChunk> = Vec::new();
    let mut header: Option<String> = None;
    let mut body = String::new();
    fn flush(header: &mut Option<String>, body: &mut String, out: &mut Vec<DiffChunk>) {
        let Some(h) = header.take() else {
            body.clear();
            return;
        };
        let binary = body
            .lines()
            .any(|l| l.starts_with("Binary files ") || l.starts_with("GIT binary patch"));
        let max_line = body.lines().map(str::len).max().unwrap_or(0);
        let path = chunk_path(&h, body);
        let mut whole = h;
        whole.push('\n');
        whole.push_str(body);
        out.push(DiffChunk {
            path,
            body: whole,
            binary,
            max_line,
        });
        body.clear();
    }
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            flush(&mut header, &mut body, &mut out);
            header = Some(line.to_string());
        } else if header.is_some() {
            body.push_str(line);
            body.push('\n');
        }
    }
    flush(&mut header, &mut body, &mut out);
    out
}

/// 設定から作る除外の判定器。**パターンは設定からしか来ない** —
/// このモジュールに既定値は 1 つも持たない。
pub struct DiffFilter {
    pats: Vec<crate::ignore::Pattern>,
    max_line: usize,
}

impl DiffFilter {
    pub fn new(cfg: &crate::config::RaceEvalConfig) -> Self {
        Self {
            pats: crate::ignore::parse(&cfg.exclude.join("\n")),
            max_line: cfg.max_line_bytes,
        }
    }

    /// パス自体が除外パターンに当たるか。
    ///
    /// `.gitignore` はディレクトリを歩きながら判定するが、ここにはファイルパスしか
    /// 無い。`target/` のようなディレクトリ指定を効かせるため、**祖先を 1 段ずつ
    /// ディレクトリとして照合**してから、最後にファイル自身を照合する。
    pub fn path_excluded(&self, rel: &str) -> bool {
        if self.pats.is_empty() {
            return false;
        }
        let comps = crate::ignore::split_rel_os(rel, cfg!(windows));
        let layer: &[&str] = &[];
        for k in 1..comps.len() {
            if crate::ignore::decide(&[(layer, &self.pats)], &comps[..k], true) == Some(true) {
                return true;
            }
        }
        crate::ignore::decide(&[(layer, &self.pats)], &comps, false) == Some(true)
    }

    /// このチャンクを評価に載せないか。載せない理由も返す (説明のため)。
    pub fn reject(&self, c: &DiffChunk) -> Option<String> {
        if self.path_excluded(&c.path) {
            return Some(tr("除外設定"));
        }
        if c.binary {
            return Some(tr("バイナリ"));
        }
        if self.max_line > 0 && c.max_line > self.max_line {
            return Some(tr("巨大な生成物"));
        }
        None
    }
}

/// 除外を適用した差分。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilteredDiff {
    pub text: String,
    pub paths: Vec<String>,
    pub excluded: Vec<String>,
    pub insertions: usize,
    pub deletions: usize,
}

/// 差分から「読ませる価値のないもの」を落とす。
pub fn filter_diff(text: &str, filter: &DiffFilter) -> FilteredDiff {
    let mut out = FilteredDiff::default();
    for c in split_diff_files(text) {
        if filter.reject(&c).is_some() {
            out.excluded.push(c.path);
            continue;
        }
        for line in c.body.lines() {
            if line.starts_with("+++") || line.starts_with("---") {
                continue;
            }
            if line.starts_with('+') {
                out.insertions += 1;
            } else if line.starts_with('-') {
                out.deletions += 1;
            }
        }
        out.paths.push(c.path);
        out.text.push_str(&c.body);
    }
    out
}

/// バイト上限で差分を切り詰める。**切ったことを本文に明示する** (黙って切らない)。
/// 戻り値は (本文, 切り詰めたか)。
pub fn truncate_diff(text: &str, max_bytes: usize) -> (String, bool) {
    if max_bytes == 0 || text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    // 文字境界まで戻す (マルチバイトの途中で切らない)。
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    // 行の途中で切ると差分として読めないので、直前の改行まで戻す。
    if let Some(nl) = text[..cut].rfind('\n') {
        cut = nl + 1;
    }
    let dropped = text.len() - cut;
    let mut s = text[..cut].to_string();
    s.push_str(&trf(
        "\n… ここで打ち切りました ({limit} バイトの上限を超えたため、残り {n} バイトは評価に載せていません)\n",
        &[
            ("limit", max_bytes.to_string()),
            ("n", dropped.to_string()),
        ],
    ));
    (s, true)
}

/// 同名プリセットが並んでも一意になる候補名を作る。
/// (勝者名で候補を引き戻すので、ここが重複すると判定が曖昧になる)
pub fn candidate_names(race: &Race) -> Vec<String> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = Vec::with_capacity(race.racers.len());
    for r in &race.racers {
        let base = format!("{} {}", r.icon, r.preset_name);
        let n = seen.entry(base.clone()).or_insert(0);
        *n += 1;
        out.push(if *n == 1 {
            base
        } else {
            format!("{base} #{n}")
        });
    }
    out
}

/// テストらしいパスか。**部分一致では見ない** — `latest.rs` を「テスト」に
/// 数えないため、パス要素ちょうどか、ファイル名の定型だけを拾う。
pub fn looks_like_test(path: &str) -> bool {
    let comps = crate::ignore::split_rel_os(path, cfg!(windows));
    if comps
        .iter()
        .any(|c| matches!(*c, "test" | "tests" | "spec" | "specs" | "__tests__"))
    {
        return true;
    }
    let Some(file) = comps.last() else {
        return false;
    };
    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
    stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with(".test")
        || stem.ends_with(".spec")
        || stem.ends_with("Test")
}

/// ドキュメントらしいパスか。
fn looks_like_doc(path: &str) -> bool {
    let comps = crate::ignore::split_rel_os(path, cfg!(windows));
    if comps.iter().any(|c| matches!(*c, "docs" | "doc")) {
        return true;
    }
    comps
        .last()
        .is_some_and(|f| f.to_ascii_lowercase().ends_with(".md"))
}

// ── 判定 (in-process) ─────────────────────────────────────────────

/// 候補 1 本の採点結果。点そのものより**内訳 (notes)** が本体で、理由文はこれで組む。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Score {
    pub idx: usize,
    pub name: String,
    pub points: i64,
    pub notes: Vec<String>,
}

/// 候補を採点する。**決定的** — 同じ入力なら必ず同じ順位になる。
///
/// 見るのは「差分から機械的に読める事実」だけ:
/// 成果があるか / 採用でマージに載るか / テストを伴うか / 他とぶつからないか /
/// 同じことをより小さく済ませているか。
pub fn score_candidates(cands: &[Candidate]) -> Vec<Score> {
    // 規模の順位付けは非空の候補だけで行う (空は比較対象にしない)。
    let mut by_size: Vec<(usize, usize)> = cands
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.is_empty())
        .map(|(i, c)| (i, c.churn()))
        .collect();
    by_size.sort_by_key(|&(i, churn)| (churn, i));

    let mut scores: Vec<Score> = Vec::with_capacity(cands.len());
    for (i, c) in cands.iter().enumerate() {
        let mut pts: i64 = 0;
        let mut notes: Vec<String> = Vec::new();
        if c.is_empty() {
            pts -= 1000;
            notes.push(tr("差分が空 (まだ成果が出ていない)"));
        } else {
            pts += 100;
            notes.push(trf(
                "{f} ファイル +{a} -{d} の成果",
                &[
                    ("f", c.files.to_string()),
                    ("a", c.insertions.to_string()),
                    ("d", c.deletions.to_string()),
                ],
            ));
        }
        if c.uncommitted {
            pts -= 400;
            notes.push(tr(
                "未コミットの変更が残っている (このままでは採用のマージに載らない)",
            ));
        }
        if !c.is_empty() && c.paths.iter().any(|p| looks_like_test(p)) {
            pts += 150;
            notes.push(tr("テストを伴っている"));
        }
        if !c.is_empty() && c.paths.iter().any(|p| looks_like_doc(p)) {
            pts += 40;
            notes.push(tr("ドキュメントを更新している"));
        }
        if c.conflicts > 0 {
            let pen = (c.conflicts as i64 * 40).min(200);
            pts -= pen;
            notes.push(trf(
                "他のレーサーと {n} ファイルで衝突している",
                &[("n", c.conflicts.to_string())],
            ));
        }
        if let Some(rank) = by_size.iter().position(|&(j, _)| j == i) {
            let bonus = match rank {
                0 => 80,
                1 => 40,
                _ => 0,
            };
            pts += bonus;
            if rank == 0 && by_size.len() >= 2 {
                notes.push(trf(
                    "最小の差分で済ませている ({n} 行)",
                    &[("n", c.churn().to_string())],
                ));
            }
        }
        if c.truncated {
            notes.push(tr("差分が上限を超えたため一部だけを見て判定している"));
        }
        scores.push(Score {
            idx: c.idx,
            name: c.name.clone(),
            points: pts,
            notes,
        });
    }
    scores
}

/// 判定役へ渡す依頼 (cmux の Crown evaluation と同じ形: `{prompt, candidates[]}`)。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EvalRequest {
    pub prompt: String,
    pub candidates: Vec<Candidate>,
}

/// 依頼を JSON にする。
///
/// **判定役の入口をここ 1 つに絞る**ための層である。in-process の判定役も外部の
/// 判定役も、受け取るのはこの JSON だけ。差し替えても経路は変わらない。
pub fn build_eval_request(prompt: &str, cands: &[Candidate]) -> Result<String, String> {
    serde_json::to_string(&EvalRequest {
        prompt: prompt.to_string(),
        candidates: cands.to_vec(),
    })
    .map_err(|_| tr("評価の依頼を組み立てられませんでした"))
}

/// in-process の判定役。依頼 JSON を読み、応答は `{"winner","reason"}` の JSON。
///
/// **外部 CLI エージェントへは投げない** (supervisor の Diagnostician と同じ原則)。
/// 判定は決定的で、同じ依頼からは必ず同じ勝者が出る。
pub fn judge_in_process(request: &str) -> Result<String, String> {
    let req: EvalRequest =
        serde_json::from_str(request).map_err(|_| tr("評価の依頼を読み取れませんでした"))?;
    let scores = score_candidates(&req.candidates);
    // 同点は添字の小さいほう (出走順) を勝ちにして、結果を安定させる。
    let Some(best) = scores
        .iter()
        .enumerate()
        .max_by_key(|(i, s)| (s.points, -(*i as i64)))
        .map(|(i, _)| i)
    else {
        return Err(tr("候補がありません"));
    };
    let win = &scores[best];
    let mut reason = win.notes.join(&tr("、"));
    if reason.is_empty() {
        reason = tr("減点が最も少ない");
    }
    // 次点との差を 1 句だけ添える (順位の根拠を示すため)。
    let runner = scores
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != best)
        .max_by_key(|(i, s)| (s.points, -(*i as i64)));
    if let Some((_, r)) = runner {
        let diff = win.points - r.points;
        reason.push_str(&trf(
            " — 次点 {name} との差は {d} 点",
            &[("name", r.name.clone()), ("d", diff.to_string())],
        ));
    }
    Ok(serde_json::json!({ "winner": win.name, "reason": reason }).to_string())
}

/// 判定役の応答を解析する。**曖昧なら必ず `Err`** (diagnostician と同じ作法)。
///
/// `Err` にする条件:
/// - JSON として読めない / 途中で切れている
/// - オブジェクトでない
/// - `winner` が無い / 文字列でない / 空
/// - `reason` が無い / 文字列でない / 空 (**根拠の無い順位は採らない**)
/// - `winner` が候補名のどれにも一致しない
pub fn parse_verdict(raw: &str, names: &[String]) -> Result<Verdict, String> {
    // コードフェンスで包んでくる判定役 (LLM) があるので、そこだけは剥がす。
    let mut body = raw.trim();
    if let Some(rest) = body.strip_prefix("```") {
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        body = rest.trim_start().trim_end_matches('`').trim();
    }
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| tr("評価結果を JSON として読めませんでした (途中で切れている可能性)"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| tr("評価結果が JSON オブジェクトではありません"))?;
    let winner = obj
        .get("winner")
        .and_then(|w| w.as_str())
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .ok_or_else(|| tr("評価結果に勝者がありません"))?;
    let reason = obj
        .get("reason")
        .and_then(|r| r.as_str())
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .ok_or_else(|| tr("評価結果に理由がありません — 根拠の無い順位は採用しません"))?;
    if !names.iter().any(|n| n == winner) {
        return Err(trf(
            "勝者 {w} は候補にいません",
            &[("w", winner.to_string())],
        ));
    }
    Ok(Verdict {
        winner: winner.to_string(),
        reason: reason.chars().take(MAX_REASON_CHARS).collect(),
    })
}

/// 除外・切り詰めの内訳を 1 行にする (何を見せていないかを必ず出す)。
pub fn collection_note(cands: &[Candidate]) -> String {
    let mut excluded: Vec<String> = Vec::new();
    for c in cands {
        for e in &c.excluded {
            if !excluded.contains(e) {
                excluded.push(e.clone());
            }
        }
    }
    let truncated = cands.iter().filter(|c| c.truncated).count();
    let mut parts: Vec<String> = Vec::new();
    if !excluded.is_empty() {
        let paths: Vec<PathBuf> = excluded.iter().map(PathBuf::from).collect();
        parts.push(trf(
            "除外 {n} 件 ({files})",
            &[
                ("n", excluded.len().to_string()),
                ("files", summarize_files(&paths, EXCLUDE_INLINE_MAX)),
            ],
        ));
    }
    if truncated > 0 {
        parts.push(trf(
            "{n} 本を上限で切り詰め",
            &[("n", truncated.to_string())],
        ));
    }
    parts.join(" / ")
}

/// 候補から勝者を決める。**採用は一切しない** — 返すのは提案だけ。
///
/// 失敗はすべて `EvalOutcome::Failed` に畳む。ここから例外も panic も出さないので、
/// 評価がどう転んでも採用・破棄・差分表示は従来どおり動く。
pub fn evaluate(prompt: &str, cands: &[Candidate]) -> EvalOutcome {
    if cands.len() < MIN_EVAL_CANDIDATES {
        return EvalOutcome::Skipped(trf(
            "候補が {n} 本しかありません — 比べるには {min} 本以上が要ります",
            &[
                ("n", cands.len().to_string()),
                ("min", MIN_EVAL_CANDIDATES.to_string()),
            ],
        ));
    }
    if cands.iter().all(Candidate::is_empty) {
        return EvalOutcome::Skipped(tr(
            "どの候補もまだ差分を出していません — もう少し走らせてから評価してください",
        ));
    }
    let names: Vec<String> = cands.iter().map(|c| c.name.clone()).collect();
    let raw = match build_eval_request(prompt, cands).and_then(|req| judge_in_process(&req)) {
        Ok(raw) => raw,
        Err(e) => return EvalOutcome::Failed(e),
    };
    match parse_verdict(&raw, &names) {
        Ok(v) => {
            let Some(c) = cands.iter().find(|c| c.name == v.winner) else {
                return EvalOutcome::Failed(tr("勝者を候補へ戻せませんでした"));
            };
            EvalOutcome::Decided {
                winner: c.idx,
                name: v.winner,
                reason: v.reason,
                note: collection_note(cands),
            }
        }
        Err(e) => EvalOutcome::Failed(e),
    }
}

/// レースの各 worktree から候補を集める (git を叩くのでワーカースレッド側で呼ぶ)。
///
/// 破棄済み / worktree が消えた racer は候補にしない。1 体でも失敗したら
/// **その 1 体だけ落として続ける** — 評価が全滅するより比べられる本数を残す。
pub fn collect_candidates(
    race: &Race,
    conflicts: &BTreeMap<usize, usize>,
    cfg: &crate::config::RaceEvalConfig,
) -> Vec<Candidate> {
    let filter = DiffFilter::new(cfg);
    let names = candidate_names(race);
    let mut budget = cfg.max_total_bytes;
    let mut out: Vec<Candidate> = Vec::new();
    for (i, r) in race.racers.iter().enumerate() {
        if matches!(r.status, RacerStatus::Discarded) || !r.dir.is_dir() {
            continue;
        }
        // core.quotepath=false: 非 ASCII のパスを 8 進エスケープさせない。
        let Ok(raw) = run_git(
            &r.dir,
            &["-c", "core.quotepath=false", "diff", &race.base_commit],
        ) else {
            continue;
        };
        let uncommitted = crate::worktree::status_entries(&r.dir)
            .map(|e| !e.is_empty())
            .unwrap_or(false);
        let f = filter_diff(&raw, &filter);
        // 1 本ぶんの上限と、全体の残り予算の**厳しいほう**で切る。
        let cap = cfg.max_diff_bytes.min(budget.max(1));
        let (text, truncated) = truncate_diff(&f.text, cap);
        budget = budget.saturating_sub(text.len());
        out.push(Candidate {
            idx: i,
            name: names
                .get(i)
                .cloned()
                .unwrap_or_else(|| racer_label(race, i)),
            branch: r.branch.clone(),
            diff: text,
            files: f.paths.len(),
            insertions: f.insertions,
            deletions: f.deletions,
            paths: f.paths,
            excluded: f.excluded,
            truncated,
            uncommitted,
            conflicts: conflicts.get(&i).copied().unwrap_or(0),
        });
    }
    out
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
    /// 勝者を評価する (**提案を出すだけ** — 採用はユーザーの明示操作のまま)。
    /// 設定 (除外パターン・上限) を持っているのは app.rs なので、実行は向こうへ頼む。
    Evaluate,
    /// ダッシュボードを畳む (worktree とブランチはそのまま残る)
    Close,
}

/// ポーリング 1 回ぶんの収穫: (racer 添字, 変更量, 触ったファイル)。
type RacerScan = (usize, DiffStat, HashSet<PathBuf>);

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
    /// 走行中の収集 (差分量 + 触ったファイル) — 別スレッドから 1 回で届く
    pending: Option<Receiver<Vec<RacerScan>>>,
    last_refresh: Option<Instant>,
    /// 触ったファイルから畳んだ衝突。ポーリング結果が届いた時だけ作り直す
    /// (毎フレームの計算も git もしない)。
    overlap: OverlapReport,
    /// 直近の勝者評価。**明示操作でしか埋まらない** (自動評価はしない)。
    eval: Option<EvalOutcome>,
    /// 飛行中の評価。UI スレッドは待たない (差分量ポーリングと同じ流儀)。
    eval_pending: Option<Receiver<EvalOutcome>>,
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
            overlap: OverlapReport::default(),
            eval: None,
            eval_pending: None,
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
        self.overlap = OverlapReport::default();
        self.eval = None;
        self.eval_pending = None;
    }

    /// ダッシュボードを畳む。worktree とブランチには触らない。
    pub fn close(&mut self) {
        self.race = None;
        self.diff_cache.clear();
        self.pending = None;
        self.last_refresh = None;
        self.overlap = OverlapReport::default();
        self.eval = None;
        self.eval_pending = None;
    }

    /// 今わかっている衝突。UI と外部 (テスト) から読むだけ。
    pub fn overlap(&self) -> &OverlapReport {
        &self.overlap
    }

    /// 直近の評価結果 (未評価なら None)。
    pub fn eval_result(&self) -> Option<&EvalOutcome> {
        self.eval.as_ref()
    }

    /// 評価が飛行中か (ボタンの見た目と二重起動の防止に使う)。
    pub fn eval_running(&self) -> bool {
        self.eval_pending.is_some()
    }

    /// racer の添字ごとの衝突ファイル数 (評価の入力に使う)。
    fn conflict_counts(&self) -> BTreeMap<usize, usize> {
        let Some(race) = &self.race else {
            return BTreeMap::new();
        };
        (0..race.racers.len())
            .map(|i| (i, self.overlap.files_for(i).len()))
            .filter(|&(_, n)| n > 0)
            .collect()
    }

    /// 勝者評価を**明示操作で 1 回だけ**走らせる。
    ///
    /// git を叩くのでワーカースレッドへ出し、UI はブロックしない。二重起動しない。
    /// 失敗しても `EvalOutcome::Failed` に畳むだけで、採用・破棄には一切触らない。
    pub fn start_eval(&mut self, cfg: &crate::config::RaceEvalConfig, ctx: &egui::Context) {
        if self.eval_pending.is_some() {
            return;
        }
        let Some(race) = self.race.clone() else {
            self.eval = Some(EvalOutcome::Failed(tr("レースがありません")));
            return;
        };
        let conflicts = self.conflict_counts();
        let cfg = cfg.clone();
        let (tx, rx) = channel();
        self.eval_pending = Some(rx);
        self.eval = None;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let cands = collect_candidates(&race, &conflicts, &cfg);
            let _ = tx.send(evaluate(&race.prompt, &cands));
            crate::perf::repaint(&ctx, "race");
        });
    }

    /// 飛行中の評価を回収する (UI スレッドは待たない)。
    fn poll_eval(&mut self) {
        let Some(rx) = &self.eval_pending else { return };
        match rx.try_recv() {
            Ok(outcome) => {
                self.eval_pending = None;
                self.eval = Some(outcome);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.eval_pending = None;
                // スレッドが答えずに落ちた場合でも、評価だけが失敗したと分かるように残す。
                if self.eval.is_none() {
                    self.eval = Some(EvalOutcome::Failed(tr(
                        "評価が途中で終了しました — もう一度お試しください",
                    )));
                }
            }
        }
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
        let title = trf("🏁 {name} の差分", &[("name", racer.preset_name.clone())]);
        Ok((title, text))
    }

    /// [採用] を押した瞬間の判断 — **2 体目の採用が本当の勝負どころ**。
    /// 既に採用済みの racer と触ったファイルが重なるなら 1 度目は警告に使い、
    /// 2 度目のクリック (`confirm_adopt`) だけがマージへ進む。塞ぎはしない。
    pub fn adopt_decision_for(&self, idx: usize) -> AdoptDecision {
        let Some(race) = &self.race else {
            return AdoptDecision::Proceed;
        };
        let Some(cand) = race.racers.get(idx).and_then(|r| r.touched.clone()) else {
            // 触ったファイルがまだ分からないうちは止めない (収集は TTL 待ち)。
            return AdoptDecision::Proceed;
        };
        let adopted: Vec<(usize, HashSet<PathBuf>)> = race
            .racers
            .iter()
            .enumerate()
            .filter(|(i, r)| *i != idx && r.status == RacerStatus::Adopted)
            .filter_map(|(i, r)| r.touched.clone().map(|t| (i, t)))
            .collect();
        let confirmed = race.racers.get(idx).is_some_and(|r| r.confirm_adopt);
        adopt_decision(&adopted, &cand, confirmed)
    }

    /// [採用] の 1 度目で警告を出す状態にする (2 度目のクリックを待つ)。
    pub fn arm_adopt_confirm(&mut self, idx: usize) {
        if let Some(r) = self.race.as_mut().and_then(|c| c.racers.get_mut(idx)) {
            r.confirm_adopt = true;
        }
    }

    /// [採用]: マージに成功したら racer を Adopted にする。
    pub fn adopt(&mut self, idx: usize) -> Result<String, String> {
        let race = self.race.as_mut().ok_or_else(|| tr("レースがありません"))?;
        let msg = adopt_racer(race, idx)?;
        if let Some(r) = race.racers.get_mut(idx) {
            r.status = RacerStatus::Adopted;
            r.confirm_discard = false;
            r.confirm_adopt = false;
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
                    r.confirm_adopt = false;
                    r.stat = None;
                    // 消えた racer はもう誰とも競合しない
                    r.touched = None;
                }
                self.recompute_overlaps();
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

    /// 触ったファイル集合から衝突を畳み直す。破棄済みの racer は数に入れない
    /// (worktree ごと消えているので、もう誰の邪魔もしない)。
    fn recompute_overlaps(&mut self) {
        let Some(race) = &self.race else {
            self.overlap = OverlapReport::default();
            return;
        };
        let sets: Vec<(usize, HashSet<PathBuf>)> = race
            .racers
            .iter()
            .enumerate()
            .filter(|(_, r)| !matches!(r.status, RacerStatus::Discarded))
            .filter_map(|(i, r)| r.touched.clone().map(|t| (i, t)))
            .collect();
        self.overlap = compute_overlaps(&sets);
    }

    /// 飛行中の収集 (差分量 + 触ったファイル) を回収する (UI スレッドは待たない)。
    fn poll(&mut self) {
        let Some(rx) = &self.pending else { return };
        match rx.try_recv() {
            Ok(list) => {
                self.pending = None;
                let mut got = false;
                if let Some(race) = &mut self.race {
                    for (i, st, touched) in list {
                        if let Some(r) = race.racers.get_mut(i) {
                            r.stat = Some(st);
                            r.touched = Some(touched);
                            got = true;
                        }
                    }
                }
                if got {
                    self.recompute_overlaps();
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.pending = None,
        }
    }

    /// TTL 切れなら収集を別スレッドで仕込む (git_panel と同じ流儀)。
    /// 衝突検出のための追加 git 呼び出しも**このスレッドに相乗り**させ、
    /// スレッドもポーリング周期も 1 本のまま保つ。
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
            let mut out: Vec<RacerScan> = Vec::new();
            for (i, dir) in jobs {
                if let Ok((st, touched)) = collect_scan(&dir, &base) {
                    out.push((i, st, touched));
                }
            }
            let _ = tx.send(out);
            crate::perf::repaint(&ctx, "race");
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
/// レースが**何も抱えていない**か (走行中でもフォームを開いてもいない)。
///
/// 真なら Cockpit はこのセクションを 1 ピクセルも描かない。見出しだけの行が
/// 常駐して 24px を食い、その分だけエージェントのタイルが痩せるため。
/// 起動口は「タスクとメッセージ」の行に間借りする。
pub fn is_idle(panel: &RacePanel) -> bool {
    panel.race.is_none() && !panel.form_open
}

pub fn race_section(
    panel: &mut RacePanel,
    ui: &mut egui::Ui,
    theme: &Theme,
    presets: &[(String, String)],
    sessions: &[(u64, bool)],
) -> Vec<RaceAction> {
    let mut acts: Vec<RaceAction> = Vec::new();
    panel.poll();
    panel.poll_eval();
    panel.maybe_refresh(ui.ctx());
    panel.sync_sessions(sessions);

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(tr("🏁 プロンプトレース"))
                .strong()
                .color(theme.accent),
        );
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
                // 🏆 勝者評価 — **押したときだけ**走る (走行中の自動評価はしない)。
                // 見出しと [閉じる] を置いた残りが狭ければアイコンだけへ縮退させる
                // (行がはみ出して見切れるより、説明をホバーへ送るほうが良い)。
                let busy = panel.eval_running();
                let compact = ui.available_width() < EVAL_BTN_FULL_W;
                let label = match (busy, compact) {
                    (true, false) => tr("🏆 評価中…"),
                    (true, true) => tr("🏆…"),
                    (false, false) => tr("🏆 勝者を評価"),
                    (false, true) => tr("🏆"),
                };
                if ui
                    .add_enabled(!busy, egui::Button::new(label))
                    .on_hover_text(tr(
                        "各 racer の差分 (ロックファイル・ビルド成果物・バイナリを除く) を比べて\
                         勝者と理由を出します。出るのは提案だけ — 採用は [採用] を押したときです",
                    ))
                    .clicked()
                {
                    acts.push(RaceAction::Evaluate);
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
        crate::perf::repaint_after(ui.ctx(), Duration::from_secs(1), "race_poll");
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
                &[
                    ("min", MIN_RACERS.to_string()),
                    ("max", MAX_RACERS.to_string()),
                ],
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

/// racer の表示名 (衝突の説明で「誰と」を指すのに使う)。
fn racer_label(race: &Race, idx: usize) -> String {
    race.racers
        .get(idx)
        .map(|r| format!("{} {}", r.icon, r.preset_name))
        .unwrap_or_else(|| trf("racer {n}", &[("n", (idx + 1).to_string())]))
}

/// 衝突しているファイルを「誰と何を」の複数行テキストにする (ツールチップ用)。
fn conflict_tooltip(race: &Race, peers: &[(usize, Vec<PathBuf>)]) -> String {
    let mut lines = vec![tr(
        "同じファイルを触っているレーサーがいます — 両方を採用すると 2 体目のマージで衝突します",
    )];
    for (other, files) in peers {
        lines.push(trf(
            "・{who}: {files}",
            &[
                ("who", racer_label(race, *other)),
                ("files", summarize_files(files, files.len())),
            ],
        ));
    }
    lines.join("\n")
}

/// racer の一覧行: アイコン+名前 / ブランチ / 状態 / 変更量 / 衝突 / 操作ボタン。
fn race_rows_ui(
    panel: &mut RacePanel,
    ui: &mut egui::Ui,
    theme: &Theme,
    acts: &mut Vec<RaceAction>,
) {
    // 1 度目の [採用] クリックで警告状態へ倒す racer。描画中は panel を
    // 不変借用しているので、ここに積んで抜けてから反映する。
    let mut arm_adopt: Vec<usize> = Vec::new();
    let Some(race) = &panel.race else { return };
    let overlap = panel.overlap();
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
    // 衝突サマリ — レビュー時ではなく「走っている今」出すのが要点。
    if !overlap.is_clean() {
        let files: Vec<PathBuf> = overlap.contended.keys().cloned().collect();
        ui.label(
            RichText::new(trf(
                "⚠ {n} ファイルが 2 体以上のレーサーで競合しています",
                &[("n", overlap.contended_count().to_string())],
            ))
            .color(theme.warn)
            .size(11.5)
            .strong(),
        )
        .on_hover_text(summarize_files(&files, files.len()));
    } else if race.racers.iter().any(|r| r.touched.is_some()) {
        ui.label(
            RichText::new(tr("✓ レーサー同士で重なっているファイルはありません"))
                .color(theme.ok)
                .size(11.0),
        );
    }
    // 🏆 勝者評価の結果。**明示操作の後にしか埋まらない**ので、未評価なら 1 行も描かない
    // (空のセクションで高さを取らない)。ここに出るのは提案で、採用は下の [採用] のまま。
    let winner_idx: Option<usize> = if panel.eval_running() {
        ui.label(
            RichText::new(tr("🏆 評価中… (各 racer の差分を集めています)"))
                .color(theme.text_dim)
                .size(11.0),
        );
        None
    } else {
        match panel.eval_result() {
            Some(EvalOutcome::Decided {
                winner,
                name,
                reason,
                note,
            }) => {
                let mut full = reason.clone();
                if !note.is_empty() {
                    full.push('\n');
                    full.push_str(note);
                }
                full.push('\n');
                full.push_str(&tr(
                    "※ これは提案です。採用 (マージ) は [採用] を押したときにだけ行われます",
                ));
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(trf("🏆 勝者 (提案): {name}", &[("name", name.clone())]))
                            .color(theme.ok)
                            .strong()
                            .size(11.5),
                    );
                    ui.label(
                        RichText::new(crate::notify::truncate_chars(reason, 72))
                            .color(theme.text_dim)
                            .size(11.0),
                    )
                    .on_hover_text(full);
                });
                Some(*winner)
            }
            Some(EvalOutcome::Skipped(why)) => {
                ui.label(
                    RichText::new(trf(
                        "🏆 評価をスキップしました — {why}",
                        &[("why", crate::notify::truncate_chars(why, 72))],
                    ))
                    .color(theme.text_dim)
                    .size(11.0),
                )
                .on_hover_text(why.clone());
                None
            }
            Some(EvalOutcome::Failed(e)) => {
                ui.label(
                    RichText::new(trf(
                        "🏆 評価に失敗しました — {e} (採用・破棄はこれまでどおり使えます)",
                        &[("e", crate::notify::truncate_chars(e, 60))],
                    ))
                    .color(theme.warn)
                    .size(11.0),
                )
                .on_hover_text(e.clone());
                None
            }
            None => None,
        }
    };
    for (i, r) in race.racers.iter().enumerate() {
        let peers = overlap.for_racer(i);
        let conflict_files = overlap.files_for(i);
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
            // 🏆 勝者バッジ — 評価が済んでいるときだけ 1 体に付く。
            if winner_idx == Some(i) {
                ui.label(RichText::new(tr("🏆 勝者")).color(theme.ok).strong().size(11.0))
                    .on_hover_text(tr(
                        "評価の提案です。採用 (マージ) は [採用] を押したときにだけ行われます",
                    ));
            }
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

            // 衝突バッジ — 取り合っているファイルがあれば警告色で名指しする。
            // 単独で触っているだけなら緑 (= このまま採用しても揉めない)。
            let touched_any = r.touched.as_ref().is_some_and(|t| !t.is_empty());
            if !conflict_files.is_empty() {
                ui.label(
                    RichText::new(trf(
                        "⚠ 衝突 {files}",
                        &[("files", summarize_files(&conflict_files, CONFLICT_INLINE_MAX))],
                    ))
                    .color(theme.warn)
                    .size(11.0),
                )
                .on_hover_text(conflict_tooltip(race, &peers));
            } else if touched_any && !matches!(r.status, RacerStatus::Discarded) {
                ui.label(
                    RichText::new(tr("✓ 単独"))
                        .color(theme.ok)
                        .size(11.0),
                )
                .on_hover_text(tr(
                    "このレーサーが触っているファイルは他の誰とも重なっていません",
                ));
            }

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
                // [採用] — マージ済み/破棄済みには出さない。
                // 既に採用済みの racer とファイルが重なる時だけ 2 段確認になる
                // ([破棄] と同じ流儀)。塞ぎはせず、知らせて 1 拍置くだけ。
                if !r.status.settled() {
                    if r.confirm_adopt {
                        if ui
                            .button(RichText::new(tr("⚠ それでも採用")).color(theme.warn))
                            .on_hover_text(tr(
                                "採用済みのレーサーと同じファイルを触っています。\
                                 続けるとマージでコンフリクトする可能性があります",
                            ))
                            .clicked()
                        {
                            acts.push(RaceAction::Adopt(i));
                        }
                    } else if ui
                        .button(RichText::new(tr("採用")).color(theme.ok))
                        .on_hover_text(trf(
                            "{branch} を {base} へマージします (コミット済みの成果だけ)\n\
                             採用済みのレーサーとファイルが重なる場合は先に警告します",
                            &[("branch", r.branch.clone()), ("base", race.base_branch.clone())],
                        ))
                        .clicked()
                    {
                        if panel.adopt_decision_for(i).may_merge() {
                            acts.push(RaceAction::Adopt(i));
                        } else {
                            arm_adopt.push(i);
                        }
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
        // 採用の警告本文は行の直下にインラインで出す (トーストは app.rs 側の
        // 持ち物なので、ダッシュボード内で完結させる)。
        let armed = if r.confirm_adopt && !r.status.settled() {
            Some(panel.adopt_decision_for(i)).filter(|d| !d.conflict_files().is_empty())
        } else {
            None
        };
        if let Some(d) = armed {
            let who: Vec<String> = d
                .conflict_with()
                .iter()
                .map(|&o| racer_label(race, o))
                .collect();
            ui.label(
                RichText::new(trf(
                    "⚠ 採用済みの {who} と {files} が重なります — もう一度 [⚠ それでも採用] を押すとマージします",
                    &[
                        ("who", who.join(", ")),
                        ("files", summarize_files(d.conflict_files(), CONFLICT_INLINE_MAX)),
                    ],
                ))
                .color(theme.warn)
                .size(11.0),
            )
            .on_hover_text(summarize_files(d.conflict_files(), d.conflict_files().len()));
        }
    }
    // ここから下は panel を書き換えるので、race / overlap の借用はここで終わり。
    let all_settled = race.racers.iter().all(|r| r.status.settled());
    for i in arm_adopt {
        panel.arm_adopt_confirm(i);
    }
    if all_settled {
        ui.label(
            RichText::new(tr(
                "全 racer が決着しました — [✕ レースを閉じる] で片付けられます",
            ))
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
    use crate::worktree::{parse_name_only_z, parse_status_z, SLUG_MAX};

    // ── スラグ / ブランチ名 ────────────────────────────────────────

    #[test]
    fn slug_from_english_prompt() {
        assert_eq!(
            slugify_prompt("Add dark mode toggle"),
            "add-dark-mode-toggle"
        );
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
        assert_eq!(
            worktree_dir_name("myrepo", "fix-bug", 3),
            "myrepo-race-fix-bug-3"
        );
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
            DiffStat {
                files: 3,
                insertions: 10,
                deletions: 2,
                untracked: 0
            }
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
        assert_eq!(
            RacerStatus::Preparing.next(Some(true)),
            RacerStatus::Running
        );
        assert_eq!(RacerStatus::Running.next(Some(false)), RacerStatus::Exited);
        // セッションが消えた (タブごと閉じた) 場合も終了扱い
        assert_eq!(RacerStatus::Running.next(None), RacerStatus::Exited);
        // 終端状態は覆らない
        assert_eq!(RacerStatus::Adopted.next(Some(true)), RacerStatus::Adopted);
        assert_eq!(
            RacerStatus::Discarded.next(Some(true)),
            RacerStatus::Discarded
        );
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
                    touched: None,
                    confirm_discard: false,
                    confirm_adopt: false,
                },
                Racer {
                    preset_name: "B".into(),
                    icon: "⚡".into(),
                    branch: "race/x-2".into(),
                    dir: PathBuf::from("b"),
                    session_id: None,
                    status: RacerStatus::Preparing,
                    stat: None,
                    touched: None,
                    confirm_discard: false,
                    confirm_adopt: false,
                },
            ],
        });
        // id 7 は終了、もう 1 体は未起動のまま
        panel.sync_sessions(&[(7, false)]);
        let race = panel.race.as_ref().unwrap();
        assert_eq!(race.racers[0].status, RacerStatus::Exited);
        assert_eq!(
            race.racers[1].status,
            RacerStatus::Preparing,
            "未起動の racer は据え置き"
        );
    }

    #[test]
    fn race_prompt_carries_prompt_and_branch() {
        let p = build_race_prompt("ダークモード追加", "race/dark-1");
        assert!(p.contains("ダークモード追加"));
        assert!(p.contains("race/dark-1"));
        assert!(p.contains("コミット"), "コミットの約束事が入る");
    }

    // ── 担当範囲ヒント (出走時の分割) ───────────────────────────────

    #[test]
    fn scoped_prompt_appends_hint() {
        let p = build_scoped_race_prompt("直して", "race/x-1", Some("src/editor/"));
        assert!(
            p.starts_with(&build_race_prompt("直して", "race/x-1")),
            "既存の文面が前置きになる"
        );
        assert!(p.contains("担当範囲: src/editor/"));
        assert!(
            p.contains("衝突") || p.contains("奪い合"),
            "住み分けの理由が入る: {p}"
        );
    }

    #[test]
    fn scoped_prompt_without_hint_is_identical() {
        // ヒント無し / 空 / 空白だけ → 既存の投入文と 1 文字も変わらない
        let base = build_race_prompt("直して", "race/x-1");
        for hint in [None, Some(""), Some("   \n ")] {
            assert_eq!(
                build_scoped_race_prompt("直して", "race/x-1", hint),
                base,
                "hint={hint:?}"
            );
        }
    }

    #[test]
    fn scoped_prompt_trims_hint() {
        let p = build_scoped_race_prompt("x", "race/x-1", Some("  src/**/*.rs  "));
        assert!(
            p.contains("担当範囲: src/**/*.rs —"),
            "前後の空白は落ちる: {p}"
        );
    }

    // ── status / name-only のパース ────────────────────────────────

    #[test]
    fn status_z_parses_records() {
        // "XY PATH\0" の連なり。末尾の空フィールドは無視される。
        let raw = " M src/a.rs\0?? new.txt\0A  src/b.rs\0";
        let got = parse_status_z(raw);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].xy, " M");
        assert_eq!(got[0].paths, vec![PathBuf::from("src/a.rs")]);
        assert!(got[1].is_untracked());
        assert_eq!(got[1].paths, vec![PathBuf::from("new.txt")]);
        assert!(!got[2].is_untracked());
    }

    #[test]
    fn status_z_rename_takes_both_paths() {
        // リネームは「新パス\0旧パス\0」— 旧パスも削除として衝突しうるので数える
        let raw = "R  src/new.rs\0src/old.rs\0 M other.rs\0";
        let got = parse_status_z(raw);
        assert_eq!(got.len(), 2, "旧パスは独立レコードにならない: {got:?}");
        assert_eq!(
            got[0].paths,
            vec![PathBuf::from("src/new.rs"), PathBuf::from("src/old.rs")]
        );
        assert_eq!(got[1].paths, vec![PathBuf::from("other.rs")]);
    }

    #[test]
    fn status_z_keeps_spaces_and_multibyte_paths() {
        // -z なので引用符もエスケープも無い — 空白や日本語がそのまま残る
        let raw = " M src/こんにちは 世界.rs\0";
        let got = parse_status_z(raw);
        assert_eq!(got[0].paths, vec![PathBuf::from("src/こんにちは 世界.rs")]);
    }

    #[test]
    fn status_z_ignores_garbage_and_empty() {
        assert!(parse_status_z("").is_empty());
        assert!(parse_status_z("\0\0").is_empty());
        assert!(parse_status_z("xx").is_empty(), "短すぎるレコードは捨てる");
        assert!(
            parse_status_z("MMno-space").is_empty(),
            "3 文字目が空白でなければ捨てる"
        );
    }

    #[test]
    fn name_only_z_splits_on_nul() {
        assert_eq!(
            parse_name_only_z("src/a.rs\0src/b.rs\0"),
            vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")]
        );
        assert!(parse_name_only_z("").is_empty());
    }

    // ── 衝突の純粋計算 (表駆動) ────────────────────────────────────

    fn fset(paths: &[&str]) -> HashSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    fn paths(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn overlaps_table() {
        struct Case {
            name: &'static str,
            sets: Vec<(usize, HashSet<PathBuf>)>,
            contended: Vec<(&'static str, Vec<usize>)>,
            pairs: Vec<(usize, usize, Vec<&'static str>)>,
        }
        let cases = vec![
            Case {
                name: "全員バラバラ (安全)",
                sets: vec![(0, fset(&["a.rs", "b.rs"])), (1, fset(&["c.rs"]))],
                contended: vec![],
                pairs: vec![],
            },
            Case {
                name: "2 体が 1 ファイルを取り合う",
                sets: vec![(0, fset(&["a.rs", "b.rs"])), (1, fset(&["b.rs", "c.rs"]))],
                contended: vec![("b.rs", vec![0, 1])],
                pairs: vec![(0, 1, vec!["b.rs"])],
            },
            Case {
                name: "3 体が同じファイル → 組は 3 つ",
                sets: vec![
                    (0, fset(&["x.rs"])),
                    (1, fset(&["x.rs"])),
                    (2, fset(&["x.rs", "y.rs"])),
                ],
                contended: vec![("x.rs", vec![0, 1, 2])],
                pairs: vec![
                    (0, 1, vec!["x.rs"]),
                    (0, 2, vec!["x.rs"]),
                    (1, 2, vec!["x.rs"]),
                ],
            },
            Case {
                name: "複数ファイルの重なりはパス昇順",
                sets: vec![
                    (0, fset(&["z.rs", "a.rs", "m.rs"])),
                    (1, fset(&["m.rs", "z.rs"])),
                ],
                contended: vec![("m.rs", vec![0, 1]), ("z.rs", vec![0, 1])],
                pairs: vec![(0, 1, vec!["m.rs", "z.rs"])],
            },
            Case {
                name: "空集合はぶつからない",
                sets: vec![(0, fset(&[])), (1, fset(&["a.rs"]))],
                contended: vec![],
                pairs: vec![],
            },
            Case {
                name: "添字が飛んでいても保たれる",
                sets: vec![(1, fset(&["a.rs"])), (3, fset(&["a.rs"]))],
                contended: vec![("a.rs", vec![1, 3])],
                pairs: vec![(1, 3, vec!["a.rs"])],
            },
        ];
        for c in cases {
            let rep = compute_overlaps(&c.sets);
            let want: BTreeMap<PathBuf, Vec<usize>> = c
                .contended
                .iter()
                .map(|(p, who)| (PathBuf::from(p), who.clone()))
                .collect();
            assert_eq!(rep.contended, want, "{}: contended", c.name);
            let want_pairs: Vec<(usize, usize, Vec<PathBuf>)> =
                c.pairs.iter().map(|(a, b, f)| (*a, *b, paths(f))).collect();
            assert_eq!(rep.pairs, want_pairs, "{}: pairs", c.name);
            assert_eq!(
                rep.is_clean(),
                c.contended.is_empty(),
                "{}: is_clean",
                c.name
            );
            assert_eq!(
                rep.contended_count(),
                c.contended.len(),
                "{}: count",
                c.name
            );
        }
    }

    #[test]
    fn overlap_views_for_one_racer() {
        let rep = compute_overlaps(&[
            (0, fset(&["a.rs", "b.rs"])),
            (1, fset(&["b.rs"])),
            (2, fset(&["a.rs", "c.rs"])),
        ]);
        // 0 は 1 とも 2 ともぶつかる
        assert_eq!(
            rep.for_racer(0),
            vec![(1, paths(&["b.rs"])), (2, paths(&["a.rs"]))]
        );
        // 1 の相手は 0 だけ
        assert_eq!(rep.for_racer(1), vec![(0, paths(&["b.rs"]))]);
        // 合併はパス昇順・重複なし
        assert_eq!(rep.files_for(0), paths(&["a.rs", "b.rs"]));
        assert_eq!(rep.files_for(2), paths(&["a.rs"]));
        // c.rs は 2 しか触っていないので競合表に載らない
        assert!(!rep.contended.contains_key(&PathBuf::from("c.rs")));
        assert!(rep.for_racer(9).is_empty(), "居ない添字は空");
    }

    // ── 採用時ガード (表駆動) ──────────────────────────────────────

    #[test]
    fn adopt_decision_table() {
        let cand = fset(&["src/app.rs", "src/race.rs"]);
        struct Case {
            name: &'static str,
            adopted: Vec<(usize, HashSet<PathBuf>)>,
            confirmed: bool,
            want: AdoptDecision,
        }
        let cases = vec![
            Case {
                name: "採用済みが居ない → 素通り",
                adopted: vec![],
                confirmed: false,
                want: AdoptDecision::Proceed,
            },
            Case {
                name: "採用済みは居るが重ならない → 素通り",
                adopted: vec![(0, fset(&["docs/readme.md"]))],
                confirmed: false,
                want: AdoptDecision::Proceed,
            },
            Case {
                name: "重なる & 未確認 → 1 度目は警告",
                adopted: vec![(0, fset(&["src/app.rs", "other.rs"]))],
                confirmed: false,
                want: AdoptDecision::NeedsConfirm {
                    with: vec![0],
                    files: paths(&["src/app.rs"]),
                },
            },
            Case {
                name: "重なる & 確認済み → 2 度目は通す",
                adopted: vec![(0, fset(&["src/app.rs"]))],
                confirmed: true,
                want: AdoptDecision::ConfirmedProceed {
                    with: vec![0],
                    files: paths(&["src/app.rs"]),
                },
            },
            Case {
                name: "複数の採用済みと重なる → 相手もファイルも昇順で束ねる",
                adopted: vec![(2, fset(&["src/race.rs"])), (0, fset(&["src/app.rs"]))],
                confirmed: false,
                want: AdoptDecision::NeedsConfirm {
                    with: vec![0, 2],
                    files: paths(&["src/app.rs", "src/race.rs"]),
                },
            },
            Case {
                name: "確認済みでも重なりが無ければ素通り扱い",
                adopted: vec![(0, fset(&["z.rs"]))],
                confirmed: true,
                want: AdoptDecision::Proceed,
            },
        ];
        for c in cases {
            let got = adopt_decision(&c.adopted, &cand, c.confirmed);
            assert_eq!(got, c.want, "{}", c.name);
            // NeedsConfirm のときだけマージを止める (塞ぐのではなく 1 拍置く)
            assert_eq!(
                got.may_merge(),
                !matches!(c.want, AdoptDecision::NeedsConfirm { .. }),
                "{}: may_merge",
                c.name
            );
        }
    }

    #[test]
    fn adopt_decision_empty_candidate_never_warns() {
        // まだ何も触っていない racer は誰の邪魔もしない
        let d = adopt_decision(&[(0, fset(&["a.rs"]))], &fset(&[]), false);
        assert_eq!(d, AdoptDecision::Proceed);
        assert!(d.conflict_files().is_empty());
        assert!(d.conflict_with().is_empty());
    }

    // ── 表示用の畳み込み ──────────────────────────────────────────

    #[test]
    fn summarize_files_truncates_with_count() {
        assert_eq!(summarize_files(&[], 2), "");
        assert_eq!(summarize_files(&paths(&["a.rs"]), 2), "a.rs");
        assert_eq!(summarize_files(&paths(&["a.rs", "b.rs"]), 2), "a.rs, b.rs");
        let s = summarize_files(&paths(&["a.rs", "b.rs", "c.rs", "d.rs"]), 2);
        assert!(s.starts_with("a.rs, b.rs"), "s={s}");
        assert!(s.contains('2'), "残り件数が出る: {s}");
        // max=0 でも 1 本は出す (空文字にはしない)
        assert!(summarize_files(&paths(&["a.rs", "b.rs"]), 0).starts_with("a.rs"));
    }

    // ── git 実フィクスチャ (git が無い環境ではスキップ) ─────────────

    /// 初期コミット済みの使い捨てリポジトリ。git が無ければ None。
    fn fixture_repo(tag: &str) -> Option<PathBuf> {
        // リポジトリを**一段深く**掘る。`race_worktree_base` は `repo.parent()` を
        // worktree の置き場にするので、リポジトリを一時ディレクトリ直下に作ると
        // worktree が**共有の $TMPDIR 直下**へ生まれる。そこに数千の残骸が溜まると
        // 並列実行時の `git worktree add` がディレクトリロックで詰まり、
        // 単独 2 秒のテストが 90 秒を超えて slow-timeout に当たる (実測)。
        // 親をテスト専用にすれば、他のテストと取り合わない。
        let base = crate::test_util::unique_temp_dir("zaivern-race", tag);
        let dir = base.join("repo");
        if std::fs::create_dir_all(&dir).is_err() {
            std::fs::remove_dir_all(&base).ok();
            return None;
        }
        if run_git(&dir, &["init", "--quiet"]).is_err() {
            std::fs::remove_dir_all(&base).ok();
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
        let Some(repo) = fixture_repo("start") else {
            return;
        };
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
        let branches = run_git(
            &race.repo,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        )
        .unwrap();
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
        let Some(repo) = fixture_repo("dirty") else {
            return;
        };
        std::fs::write(repo.join("a.txt"), "modified\n").expect("dirty write");
        let err = start_race(&repo, "x y", &two_presets()).expect_err("汚れたツリーでは開始しない");
        assert!(err.contains("未コミット"), "err={err}");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn adopt_fast_forwards_committed_work() {
        let Some(repo) = fixture_repo("adopt") else {
            return;
        };
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
        let branches = run_git(
            &race.repo,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        )
        .unwrap();
        assert!(
            !branches.contains(&race.racers[1].branch),
            "ブランチも消える"
        );
        cleanup(&race);
    }

    #[test]
    fn adopt_refuses_uncommitted_racer_changes() {
        let Some(repo) = fixture_repo("adopt-dirty") else {
            return;
        };
        let race = start_race(&repo, "feature y", &two_presets()).expect("開始");
        std::fs::write(race.racers[0].dir.join("a.txt"), "uncommitted\n").expect("write");
        let err = adopt_racer(&race, 0).expect_err("未コミットの成果は採用しない");
        assert!(err.contains("コミット"), "err={err}");
        cleanup(&race);
    }

    #[test]
    fn adopt_conflict_aborts_and_leaves_base_clean() {
        let Some(repo) = fixture_repo("conflict") else {
            return;
        };
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
        let Some(repo) = fixture_repo("discard") else {
            return;
        };
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
        let Some(repo) = fixture_repo("panel-discard") else {
            return;
        };
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

    // ── 触ったファイルの収集 (実 git フィクスチャ) ─────────────────

    #[test]
    fn collect_scan_sees_committed_and_uncommitted() {
        let Some(repo) = fixture_repo("touched") else {
            return;
        };
        // ベースに 3 ファイル置いてからレースを始める
        for f in ["shared.rs", "solo1.rs", "solo2.rs"] {
            std::fs::write(repo.join(f), "base\n").expect("write");
        }
        git_ok(&repo, &["add", "."]);
        commit(&repo, "seed");
        let race = start_race(&repo, "touch test", &two_presets()).expect("開始");
        let (d0, d1) = (race.racers[0].dir.clone(), race.racers[1].dir.clone());

        // racer 0: shared.rs を「コミット済み」で変更 + new0.txt を未追跡で置く
        std::fs::write(d0.join("shared.rs"), "from racer0\n").expect("write");
        std::fs::write(d0.join("solo1.rs"), "only racer0\n").expect("write");
        git_ok(&d0, &["add", "."]);
        commit(&d0, "racer0 work");
        std::fs::write(d0.join("new0.txt"), "untracked\n").expect("write");

        // racer 1: shared.rs を「未コミット」で変更 (重なる) + solo2.rs は自分だけ
        std::fs::write(d1.join("shared.rs"), "from racer1\n").expect("write");
        std::fs::write(d1.join("solo2.rs"), "only racer1\n").expect("write");

        let (st0, t0) = collect_scan(&d0, &race.base_commit).expect("scan 0");
        let (st1, t1) = collect_scan(&d1, &race.base_commit).expect("scan 1");
        assert_eq!(st0.untracked, 1, "未追跡は 1 本");
        assert_eq!(st1.untracked, 0);
        // コミット済み (shared/solo1) も未追跡 (new0) も同じ集合に入る
        assert_eq!(
            t0,
            fset(&["shared.rs", "solo1.rs", "new0.txt"]),
            "racer0 の集合 t0={t0:?}"
        );
        assert_eq!(
            t1,
            fset(&["shared.rs", "solo2.rs"]),
            "racer1 の集合 t1={t1:?}"
        );

        // 重なるのは shared.rs だけ — solo1/solo2/new0 は安全
        let rep = compute_overlaps(&[(0, t0), (1, t1)]);
        assert_eq!(rep.contended_count(), 1);
        assert_eq!(rep.files_for(0), paths(&["shared.rs"]));
        assert_eq!(rep.pairs, vec![(0, 1, paths(&["shared.rs"]))]);
        cleanup(&race);
    }

    #[test]
    fn collect_scan_disjoint_racers_have_no_overlap() {
        let Some(repo) = fixture_repo("disjoint") else {
            return;
        };
        let race = start_race(&repo, "disjoint test", &two_presets()).expect("開始");
        std::fs::write(race.racers[0].dir.join("only-a.rs"), "a\n").expect("write");
        std::fs::write(race.racers[1].dir.join("only-b.rs"), "b\n").expect("write");
        let (_, t0) = collect_scan(&race.racers[0].dir, &race.base_commit).expect("scan 0");
        let (_, t1) = collect_scan(&race.racers[1].dir, &race.base_commit).expect("scan 1");
        let rep = compute_overlaps(&[(0, t0), (1, t1)]);
        assert!(rep.is_clean(), "重なりなし: {rep:?}");
        assert!(rep.for_racer(0).is_empty());
        cleanup(&race);
    }

    #[test]
    fn collect_scan_tracks_renames_on_both_paths() {
        let Some(repo) = fixture_repo("rename") else {
            return;
        };
        let race = start_race(&repo, "rename test", &two_presets()).expect("開始");
        let d0 = race.racers[0].dir.clone();
        // a.txt (フィクスチャの初期ファイル) を index 上でリネームする
        git_ok(&d0, &["mv", "a.txt", "b.txt"]);
        let (_, t0) = collect_scan(&d0, &race.base_commit).expect("scan");
        assert!(t0.contains(&PathBuf::from("b.txt")), "新パス t0={t0:?}");
        assert!(
            t0.contains(&PathBuf::from("a.txt")),
            "旧パスも触った扱い t0={t0:?}"
        );
        cleanup(&race);
    }

    // ── 採用時ガードのパネル結線 (実 git フィクスチャ) ──────────────

    #[test]
    fn panel_adopt_guard_warns_on_second_overlapping_adopt() {
        let Some(repo) = fixture_repo("guard") else {
            return;
        };
        let race = start_race(&repo, "guard test", &two_presets()).expect("開始");
        let (d0, d1) = (race.racers[0].dir.clone(), race.racers[1].dir.clone());
        // 2 体とも a.txt を触る (別の行なのでマージ自体は通るが、取り合いではある)
        std::fs::write(d0.join("a.txt"), "base\nfrom0\n").expect("write");
        git_ok(&d0, &["add", "."]);
        commit(&d0, "racer0");
        std::fs::write(d1.join("a.txt"), "base\nfrom1\n").expect("write");
        git_ok(&d1, &["add", "."]);
        commit(&d1, "racer1");

        let mut panel = RacePanel::new();
        panel.begin(race);
        // ポーリングの中身を同期で流し込む (UI スレッドの経路は別テストの範囲外)
        {
            let base = panel.race.as_ref().unwrap().base_commit.clone();
            let dirs: Vec<PathBuf> = panel
                .race
                .as_ref()
                .unwrap()
                .racers
                .iter()
                .map(|r| r.dir.clone())
                .collect();
            for (i, dir) in dirs.iter().enumerate() {
                let (st, touched) = collect_scan(dir, &base).expect("scan");
                let r = &mut panel.race.as_mut().unwrap().racers[i];
                r.stat = Some(st);
                r.touched = Some(touched);
            }
            panel.recompute_overlaps();
        }
        assert_eq!(
            panel.overlap().contended_count(),
            1,
            "a.txt を取り合っている"
        );

        // 1 体目の採用は警告なし (採用済みが居ないので)
        assert_eq!(panel.adopt_decision_for(0), AdoptDecision::Proceed);
        panel.adopt(0).expect("1 体目の採用");

        // 2 体目は同じファイルを触っているので 1 度目は確認を求める
        let d = panel.adopt_decision_for(1);
        assert!(!d.may_merge(), "1 度目は止まる: {d:?}");
        assert_eq!(d.conflict_with(), &[0]);
        assert_eq!(d.conflict_files(), paths(&["a.txt"]).as_slice());

        // 確認フラグを立てた 2 度目は通る (塞がずに知らせるだけ)
        panel.arm_adopt_confirm(1);
        assert!(panel.adopt_decision_for(1).may_merge(), "2 度目は通す");
        let snapshot = panel.race.as_ref().unwrap().clone();
        cleanup(&snapshot);
    }

    #[test]
    fn panel_adopt_guard_silent_when_disjoint() {
        let Some(repo) = fixture_repo("guard-disjoint") else {
            return;
        };
        let race = start_race(&repo, "guard disjoint", &two_presets()).expect("開始");
        let mut panel = RacePanel::new();
        panel.begin(race);
        {
            let r = &mut panel.race.as_mut().unwrap().racers[0];
            r.status = RacerStatus::Adopted;
            r.touched = Some(fset(&["src/one.rs"]));
        }
        panel.race.as_mut().unwrap().racers[1].touched = Some(fset(&["src/two.rs"]));
        panel.recompute_overlaps();
        assert!(panel.overlap().is_clean());
        assert_eq!(
            panel.adopt_decision_for(1),
            AdoptDecision::Proceed,
            "重ならなければ黙って通す"
        );
        let snapshot = panel.race.as_ref().unwrap().clone();
        cleanup(&snapshot);
    }

    #[test]
    fn discard_clears_touched_and_overlap() {
        let Some(repo) = fixture_repo("discard-overlap") else {
            return;
        };
        let race = start_race(&repo, "discard overlap", &two_presets()).expect("開始");
        let mut panel = RacePanel::new();
        panel.begin(race);
        for i in 0..2 {
            panel.race.as_mut().unwrap().racers[i].touched = Some(fset(&["shared.rs"]));
        }
        panel.recompute_overlaps();
        assert_eq!(panel.overlap().contended_count(), 1);
        // 破棄した racer はもう誰とも競合しない → サマリが消える
        panel.discard(1, false).expect("破棄");
        assert!(
            panel.overlap().is_clean(),
            "破棄後は競合なし: {:?}",
            panel.overlap()
        );
        assert!(panel.race.as_ref().unwrap().racers[1].touched.is_none());
        let snapshot = panel.race.as_ref().unwrap().clone();
        cleanup(&snapshot);
    }

    // ═══════════════════════════════════════════════════════════════
    // 🏆 勝者評価
    // ═══════════════════════════════════════════════════════════════

    use crate::config::RaceEvalConfig;

    /// 純粋ロジックのテスト用に候補を 1 本でっち上げる。
    fn cand(idx: usize, name: &str, paths: &[&str], ins: usize, del: usize) -> Candidate {
        Candidate {
            idx,
            name: name.to_string(),
            branch: format!("race/x-{}", idx + 1),
            diff: if paths.is_empty() {
                String::new()
            } else {
                format!("diff --git a/{p} b/{p}\n+x\n", p = paths[0])
            },
            files: paths.len(),
            insertions: ins,
            deletions: del,
            paths: paths.iter().map(|p| p.to_string()).collect(),
            excluded: Vec::new(),
            truncated: false,
            uncommitted: false,
            conflicts: 0,
        }
    }

    /// 1 ファイルぶんの unified diff を組む (git の出力そのままの形)。
    fn diff_file(path: &str, body: &str) -> String {
        format!(
            "diff --git a/{path} b/{path}\nindex 111..222 100644\n--- a/{path}\n+++ b/{path}\n@@ -0,0 +1 @@\n{body}\n"
        )
    }

    // ── 差分の切り分け ────────────────────────────────────────────

    #[test]
    fn 差分をファイル単位に切り分ける() {
        let text = format!(
            "{}{}",
            diff_file("src/a.rs", "+fn a() {}"),
            diff_file("docs/b.md", "+# b")
        );
        let chunks = split_diff_files(&text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].path, "src/a.rs");
        assert_eq!(chunks[1].path, "docs/b.md");
        assert!(chunks.iter().all(|c| !c.binary));
        // 本文にはヘッダ行も残る (そのまま判定役へ渡せる形)
        assert!(chunks[0].body.starts_with("diff --git a/src/a.rs"));
    }

    #[test]
    fn 空白を含むパスもハンク内の記号も取り違えない() {
        // `+++ b/<path>` は行末までがパスなので空白を含んでも壊れない。
        // ハンクに入った後の `+++ わな` は本文なのでパスに使わない。
        let text = "diff --git a/my dir/a b.txt b/my dir/a b.txt\n\
                    --- a/my dir/a b.txt\n\
                    +++ b/my dir/a b.txt\n\
                    @@ -1 +1 @@\n\
                    +++ わな\n";
        let chunks = split_diff_files(text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].path, "my dir/a b.txt");
    }

    #[test]
    fn バイナリのチャンクを見分ける() {
        let text = "diff --git a/logo.png b/logo.png\n\
                    index 111..222 100644\n\
                    Binary files a/logo.png and b/logo.png differ\n";
        let chunks = split_diff_files(text);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].binary, "Binary files 行でバイナリと分かる");
    }

    // ── 除外は設定から来る ────────────────────────────────────────

    #[test]
    fn 除外パターンは設定から来る() {
        // 既定はロックファイル・ビルド成果物を落とす
        let f = DiffFilter::new(&RaceEvalConfig::default());
        assert!(f.path_excluded("Cargo.lock"));
        assert!(f.path_excluded("target/debug/zai"));
        assert!(!f.path_excluded("src/race.rs"));

        // 設定を空にすれば**何も落ちない** (= 判定はコードでなく設定が握っている)
        let empty = RaceEvalConfig {
            exclude: Vec::new(),
            ..RaceEvalConfig::default()
        };
        let f = DiffFilter::new(&empty);
        assert!(!f.path_excluded("Cargo.lock"));
        assert!(!f.path_excluded("target/debug/zai"));

        // 設定に書いたものだけが落ちる
        let custom = RaceEvalConfig {
            exclude: vec!["secret/".to_string(), "*.golden".to_string()],
            ..RaceEvalConfig::default()
        };
        let f = DiffFilter::new(&custom);
        assert!(f.path_excluded("secret/keys.txt"));
        assert!(f.path_excluded("tests/out.golden"));
        assert!(!f.path_excluded("Cargo.lock"), "設定に無いものは落とさない");
    }

    #[test]
    fn ロックファイルとビルド成果物は評価に載せない() {
        let text = format!(
            "{}{}{}{}",
            diff_file("Cargo.lock", "+checksum = \"x\""),
            diff_file("node_modules/left-pad/index.js", "+module.exports"),
            diff_file("target/debug/build.rs", "+generated"),
            diff_file("src/race.rs", "+fn win() {}")
        );
        let f = DiffFilter::new(&RaceEvalConfig::default());
        let out = filter_diff(&text, &f);
        assert_eq!(out.paths, vec!["src/race.rs".to_string()]);
        assert_eq!(out.excluded.len(), 3, "落とした 3 件は黙って消さず数える");
        assert!(!out.text.contains("Cargo.lock"));
        assert!(!out.text.contains("node_modules"));
        assert!(out.text.contains("src/race.rs"));
    }

    #[test]
    fn バイナリと巨大な生成物は評価に載せない() {
        let big = "+".to_string() + &"a".repeat(9000);
        let text = format!(
            "diff --git a/logo.png b/logo.png\nBinary files a/logo.png and b/logo.png differ\n{}{}",
            diff_file("bundle.js", &big),
            diff_file("src/a.rs", "+fn a() {}")
        );
        let cfg = RaceEvalConfig {
            max_line_bytes: 4096,
            ..RaceEvalConfig::default()
        };
        let out = filter_diff(&text, &DiffFilter::new(&cfg));
        assert_eq!(out.paths, vec!["src/a.rs".to_string()]);
        assert!(out.excluded.contains(&"logo.png".to_string()));
        assert!(out.excluded.contains(&"bundle.js".to_string()));
    }

    // ── 切り詰め ─────────────────────────────────────────────────

    #[test]
    fn 上限を超えた差分は切り詰めて明示する() {
        let text = (0..200).map(|i| format!("+line {i}\n")).collect::<String>();
        let (cut, truncated) = truncate_diff(&text, 200);
        assert!(truncated);
        assert!(cut.contains("打ち切り"), "黙って切らない: {cut}");
        assert!(cut.contains("バイト"), "何バイト捨てたかを出す: {cut}");
        // 上限に収まるものは触らない
        let (same, t2) = truncate_diff("short\n", 4096);
        assert_eq!(same, "short\n");
        assert!(!t2);
    }

    #[test]
    fn 切り詰めはマルチバイトの途中で割らない() {
        let text = "あいうえお\nかきくけこ\n".to_string();
        let (cut, truncated) = truncate_diff(&text, 20);
        assert!(truncated);
        // String として成立している = char 境界を割っていない
        assert!(cut.starts_with("あいうえお"));
    }

    // ── 応答の解析 ───────────────────────────────────────────────

    fn names2() -> Vec<String> {
        vec!["👾 Claude".to_string(), "⚡ Codex".to_string()]
    }

    #[test]
    fn 評価結果のパース_正常() {
        let raw = r#"{"winner":"⚡ Codex","reason":"テストを伴っている"}"#;
        let v = parse_verdict(raw, &names2()).expect("読める");
        assert_eq!(v.winner, "⚡ Codex");
        assert_eq!(v.reason, "テストを伴っている");
    }

    #[test]
    fn 評価結果のパース_コードフェンス付きでも読める() {
        let raw = "```json\n{\"winner\":\"👾 Claude\",\"reason\":\"最小の差分\"}\n```";
        let v = parse_verdict(raw, &names2()).expect("フェンスは剥がす");
        assert_eq!(v.winner, "👾 Claude");
    }

    #[test]
    fn 評価結果のパース_壊れた入力() {
        for raw in ["", "   ", "not json at all", "[1,2,3]", "\"just a string\""] {
            assert!(
                parse_verdict(raw, &names2()).is_err(),
                "JSON として読めないものは必ず Err: {raw:?}"
            );
        }
    }

    #[test]
    fn 評価結果のパース_途中で切れた応答() {
        let raw = r#"{"winner":"⚡ Codex","reason":"テストを伴っ"#;
        let e = parse_verdict(raw, &names2()).expect_err("途中で切れていたら黙る");
        assert!(e.contains("JSON"), "e={e}");
    }

    #[test]
    fn 評価結果のパース_勝者が候補に無い名前() {
        let raw = r#"{"winner":"🦊 Gemini","reason":"速かった"}"#;
        let e = parse_verdict(raw, &names2()).expect_err("候補外は採らない");
        assert!(e.contains("候補にいません"), "e={e}");
    }

    #[test]
    fn 評価結果のパース_理由が空() {
        for raw in [
            r#"{"winner":"⚡ Codex","reason":""}"#,
            r#"{"winner":"⚡ Codex","reason":"   "}"#,
            r#"{"winner":"⚡ Codex"}"#,
            r#"{"winner":"⚡ Codex","reason":42}"#,
        ] {
            let e = parse_verdict(raw, &names2()).expect_err("根拠の無い順位は採らない");
            assert!(e.contains("理由"), "raw={raw} e={e}");
        }
        // 勝者側が空でも同じく黙る
        assert!(parse_verdict(r#"{"winner":"","reason":"x"}"#, &names2()).is_err());
    }

    #[test]
    fn 評価結果の理由は上限で打ち切られる() {
        let long = "あ".repeat(MAX_REASON_CHARS + 50);
        let raw = serde_json::json!({ "winner": "⚡ Codex", "reason": long }).to_string();
        let v = parse_verdict(&raw, &names2()).expect("読める");
        assert_eq!(v.reason.chars().count(), MAX_REASON_CHARS);
    }

    // ── 判定 ─────────────────────────────────────────────────────

    #[test]
    fn 候補が2本未満なら評価をスキップする() {
        match evaluate("p", &[]) {
            EvalOutcome::Skipped(why) => assert!(why.contains("0 本"), "why={why}"),
            other => panic!("0 本はスキップ: {other:?}"),
        }
        match evaluate("p", &[cand(0, "👾 Claude", &["src/a.rs"], 10, 0)]) {
            EvalOutcome::Skipped(why) => {
                assert!(why.contains("1 本"), "why={why}");
                assert!(why.contains("2 本以上"), "必要本数も伝える: {why}");
            }
            other => panic!("1 本はスキップ: {other:?}"),
        }
    }

    #[test]
    fn 候補が2本なら勝者と理由が出る() {
        let cands = [
            cand(0, "👾 Claude", &["src/a.rs"], 400, 200),
            cand(1, "⚡ Codex", &["src/a.rs", "tests/a_test.rs"], 40, 5),
        ];
        match evaluate("dark mode", &cands) {
            EvalOutcome::Decided {
                winner,
                name,
                reason,
                ..
            } => {
                assert_eq!(winner, 1, "テスト付き + 小さい差分が勝つ");
                assert_eq!(name, "⚡ Codex");
                assert!(!reason.trim().is_empty(), "理由は必ず出る");
                assert!(reason.contains("テスト"), "根拠が読める: {reason}");
            }
            other => panic!("2 本なら決まる: {other:?}"),
        }
    }

    #[test]
    fn 候補が3本以上でも勝者は1本に決まる() {
        let cands = [
            cand(0, "👾 Claude", &["src/a.rs"], 500, 400),
            cand(1, "⚡ Codex", &["src/a.rs"], 300, 100),
            cand(2, "✨ Gemini", &["src/a.rs", "tests/t.rs"], 60, 10),
            cand(3, "🦆 Goose", &["src/a.rs"], 80, 10),
        ];
        match evaluate("x", &cands) {
            EvalOutcome::Decided { winner, name, .. } => {
                assert_eq!(winner, 2);
                assert_eq!(name, "✨ Gemini");
            }
            other => panic!("決まるはず: {other:?}"),
        }
    }

    #[test]
    fn 差分が空の候補は勝者にならない() {
        let mut empty = cand(0, "👾 Claude", &[], 0, 0);
        empty.diff.clear();
        let cands = [empty, cand(1, "⚡ Codex", &["src/a.rs"], 12, 0)];
        match evaluate("x", &cands) {
            EvalOutcome::Decided { winner, .. } => assert_eq!(winner, 1),
            other => panic!("片方に成果があれば決まる: {other:?}"),
        }
        // 全部空ならスキップ (比べる材料がない)
        let mut a = cand(0, "👾 Claude", &[], 0, 0);
        let mut b = cand(1, "⚡ Codex", &[], 0, 0);
        a.diff.clear();
        b.diff.clear();
        match evaluate("x", &[a, b]) {
            EvalOutcome::Skipped(why) => assert!(why.contains("差分"), "why={why}"),
            other => panic!("全部空はスキップ: {other:?}"),
        }
    }

    #[test]
    fn 未コミットの候補は勝てない() {
        // [採用] はコミット済みの成果しかマージできないので、そこを重く見る。
        let mut dirty = cand(0, "👾 Claude", &["src/a.rs", "tests/t.rs"], 30, 0);
        dirty.uncommitted = true;
        let cands = [dirty, cand(1, "⚡ Codex", &["src/a.rs"], 200, 100)];
        match evaluate("x", &cands) {
            EvalOutcome::Decided { winner, .. } => assert_eq!(winner, 1),
            other => panic!("決まるはず: {other:?}"),
        }
    }

    #[test]
    fn 評価は決定的で同じ入力から同じ勝者を出す() {
        let cands = [
            cand(0, "👾 Claude", &["src/a.rs"], 100, 0),
            cand(1, "⚡ Codex", &["src/b.rs"], 100, 0),
        ];
        let first = evaluate("x", &cands);
        for _ in 0..5 {
            assert_eq!(evaluate("x", &cands), first, "同じ入力なら同じ結果");
        }
        // 同点は出走順 (添字の小さいほう) が勝つ
        match first {
            EvalOutcome::Decided { winner, .. } => assert_eq!(winner, 0),
            other => panic!("決まるはず: {other:?}"),
        }
    }

    #[test]
    fn 依頼は判定役の唯一の入口になっている() {
        let cands = [
            cand(0, "👾 Claude", &["src/a.rs"], 10, 0),
            cand(1, "⚡ Codex", &["src/b.rs"], 20, 0),
        ];
        let req = build_eval_request("dark mode", &cands).expect("組める");
        let parsed: serde_json::Value = serde_json::from_str(&req).expect("JSON");
        assert_eq!(parsed["prompt"], "dark mode");
        assert_eq!(parsed["candidates"].as_array().map(Vec::len), Some(2));
        // 判定役は依頼 JSON だけを受け取る
        let raw = judge_in_process(&req).expect("判定できる");
        let names: Vec<String> = cands.iter().map(|c| c.name.clone()).collect();
        assert!(parse_verdict(&raw, &names).is_ok());
        // 壊れた依頼では黙って Err (捏造しない)
        assert!(judge_in_process("{ broken").is_err());
    }

    #[test]
    fn 除外と切り詰めの内訳は必ず出す() {
        let mut a = cand(0, "👾 Claude", &["src/a.rs"], 10, 0);
        a.excluded = vec!["Cargo.lock".into(), "target/x".into()];
        a.truncated = true;
        let b = cand(1, "⚡ Codex", &["src/b.rs"], 10, 0);
        let note = collection_note(&[a, b]);
        assert!(note.contains("除外 2 件"), "note={note}");
        assert!(note.contains("切り詰め"), "note={note}");
        // 何も落としていなければ空 (空の行を描かせない)
        let clean = collection_note(&[cand(0, "x", &["a.rs"], 1, 0)]);
        assert!(clean.is_empty());
    }

    // ── 補助の純関数 ─────────────────────────────────────────────

    #[test]
    fn テスト判定は部分一致で拾わない() {
        assert!(looks_like_test("tests/race.rs"));
        assert!(looks_like_test("src/__tests__/a.js"));
        assert!(looks_like_test("src/test_race.py"));
        assert!(looks_like_test("src/race_test.go"));
        assert!(looks_like_test("web/app.spec.ts"));
        // 「test」を含むだけの名前は数えない
        assert!(!looks_like_test("src/latest.rs"));
        assert!(!looks_like_test("src/contest.rs"));
        assert!(!looks_like_test("src/testing_utils_helper.rs"));
    }

    // ── git 実フィクスチャ ────────────────────────────────────────

    fn three_presets() -> Vec<(String, String)> {
        vec![
            ("👾".to_string(), "Claude".to_string()),
            ("⚡".to_string(), "Codex".to_string()),
            ("✨".to_string(), "Gemini".to_string()),
        ]
    }

    /// racer の worktree にファイルを置いてコミットする。
    fn racer_commit(r: &Racer, files: &[(&str, &str)], msg: &str) {
        for (name, body) in files {
            let path = r.dir.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(&path, body).expect("write");
        }
        git_ok(&r.dir, &["add", "."]);
        commit(&r.dir, msg);
    }

    #[test]
    fn 候補収集は2体ぶんの差分を集めロックファイルと成果物を落とす() {
        let Some(repo) = fixture_repo("cand2") else {
            return;
        };
        let race = start_race(&repo, "eval two", &two_presets()).expect("開始");
        racer_commit(
            &race.racers[0],
            &[
                ("src/win.rs", "fn win() {}\n"),
                ("Cargo.lock", "# generated\nname = \"x\"\n"),
                ("target/debug/out", "binary-ish\n"),
            ],
            "work 1",
        );
        racer_commit(
            &race.racers[1],
            &[("src/other.rs", "fn other() {}\n")],
            "work 2",
        );
        let cands = collect_candidates(&race, &BTreeMap::new(), &RaceEvalConfig::default());
        assert_eq!(cands.len(), 2, "2 体とも候補になる");
        assert_eq!(cands[0].idx, 0);
        assert_eq!(cands[0].paths, vec!["src/win.rs".to_string()]);
        assert!(
            cands[0].excluded.contains(&"Cargo.lock".to_string()),
            "excluded={:?}",
            cands[0].excluded
        );
        assert!(cands[0].excluded.iter().any(|p| p.starts_with("target/")));
        assert!(!cands[0].diff.contains("Cargo.lock"));
        assert!(cands[0].diff.contains("src/win.rs"));
        assert!(cands.iter().all(|c| !c.uncommitted));
        // 2 本そろえば勝者が出る
        assert!(matches!(
            evaluate(&race.prompt, &cands),
            EvalOutcome::Decided { .. }
        ));
        cleanup(&race);
    }

    #[test]
    fn 候補収集は3体以上でも全員を集める() {
        let Some(repo) = fixture_repo("cand3") else {
            return;
        };
        let race = start_race(&repo, "eval three", &three_presets()).expect("開始");
        for (i, r) in race.racers.iter().enumerate() {
            racer_commit(r, &[("src/a.rs", &format!("fn a{i}() {{}}\n"))], "work");
        }
        let cands = collect_candidates(&race, &BTreeMap::new(), &RaceEvalConfig::default());
        assert_eq!(cands.len(), 3);
        assert_eq!(
            cands.iter().map(|c| c.idx).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        match evaluate(&race.prompt, &cands) {
            EvalOutcome::Decided { winner, reason, .. } => {
                assert!(winner < 3);
                assert!(!reason.is_empty());
            }
            other => panic!("3 本なら決まる: {other:?}"),
        }
        cleanup(&race);
    }

    #[test]
    fn 破棄済みは候補にならず1本以下ならスキップされる() {
        let Some(repo) = fixture_repo("cand1") else {
            return;
        };
        let race = start_race(&repo, "eval one", &two_presets()).expect("開始");
        racer_commit(&race.racers[0], &[("src/a.rs", "fn a() {}\n")], "work");
        let mut panel = RacePanel::new();
        panel.begin(race);
        panel.discard(1, true).expect("破棄");
        let race = panel.race.as_ref().expect("レース").clone();
        let cands = collect_candidates(&race, &BTreeMap::new(), &RaceEvalConfig::default());
        assert_eq!(cands.len(), 1, "破棄済みは候補にしない");
        match evaluate(&race.prompt, &cands) {
            EvalOutcome::Skipped(why) => assert!(why.contains("1 本"), "why={why}"),
            other => panic!("1 本はスキップ: {other:?}"),
        }
        cleanup(&race);
    }

    #[test]
    fn 差分が空のracerも候補には載る() {
        let Some(repo) = fixture_repo("candempty") else {
            return;
        };
        let race = start_race(&repo, "eval empty", &two_presets()).expect("開始");
        racer_commit(&race.racers[0], &[("src/a.rs", "fn a() {}\n")], "work");
        // racer 1 は何もしていない
        let cands = collect_candidates(&race, &BTreeMap::new(), &RaceEvalConfig::default());
        assert_eq!(cands.len(), 2);
        assert!(!cands[0].is_empty());
        assert!(cands[1].is_empty(), "何もしていない racer は空の候補");
        assert_eq!(cands[1].files, 0);
        match evaluate(&race.prompt, &cands) {
            EvalOutcome::Decided { winner, .. } => assert_eq!(winner, 0),
            other => panic!("成果のあるほうが勝つ: {other:?}"),
        }
        cleanup(&race);
    }

    #[test]
    fn 候補収集は上限で切り詰めて明示する() {
        let Some(repo) = fixture_repo("candcut") else {
            return;
        };
        let race = start_race(&repo, "eval big", &two_presets()).expect("開始");
        let big: String = (0..400).map(|i| format!("line {i}\n")).collect();
        racer_commit(&race.racers[0], &[("src/big.rs", &big)], "big");
        racer_commit(&race.racers[1], &[("src/small.rs", "fn s() {}\n")], "small");
        let cfg = RaceEvalConfig {
            max_diff_bytes: 512,
            ..RaceEvalConfig::default()
        };
        let cands = collect_candidates(&race, &BTreeMap::new(), &cfg);
        assert!(cands[0].truncated, "上限を超えたら切り詰める");
        assert!(
            cands[0].diff.contains("打ち切り"),
            "切ったことを本文で明示する: {}",
            cands[0].diff
        );
        assert!(!cands[1].truncated, "収まっているほうは触らない");
        // 内訳にも出る
        assert!(collection_note(&cands).contains("切り詰め"));
        cleanup(&race);
    }

    #[test]
    fn 未コミットの成果は候補にそう記録される() {
        let Some(repo) = fixture_repo("canddirty") else {
            return;
        };
        let race = start_race(&repo, "eval dirty", &two_presets()).expect("開始");
        std::fs::write(race.racers[0].dir.join("a.txt"), "uncommitted\n").expect("write");
        racer_commit(&race.racers[1], &[("src/b.rs", "fn b() {}\n")], "clean");
        let cands = collect_candidates(&race, &BTreeMap::new(), &RaceEvalConfig::default());
        assert!(cands[0].uncommitted, "未コミットを見落とさない");
        assert!(!cands[1].uncommitted);
        cleanup(&race);
    }

    #[test]
    fn 候補名は同名プリセットでも一意になる() {
        let Some(repo) = fixture_repo("canddup") else {
            return;
        };
        let dup = vec![
            ("👾".to_string(), "Claude".to_string()),
            ("👾".to_string(), "Claude".to_string()),
        ];
        let race = start_race(&repo, "eval dup", &dup).expect("開始");
        let names = candidate_names(&race);
        assert_eq!(names[0], "👾 Claude");
        assert_eq!(names[1], "👾 Claude #2");
        assert_ne!(names[0], names[1], "勝者名で引き戻せる必要がある");
        cleanup(&race);
    }

    #[test]
    fn 評価が失敗しても採用フローは従来どおり動く() {
        let Some(repo) = fixture_repo("evalfail") else {
            return;
        };
        let race = start_race(&repo, "eval fail", &two_presets()).expect("開始");
        racer_commit(&race.racers[0], &[("win.txt", "winner\n")], "winner work");
        let mut panel = RacePanel::new();
        panel.begin(race);
        // 評価だけが失敗した状態にする (収集も判定も壊れたのと同じ)
        panel.eval = Some(EvalOutcome::Failed("判定役が答えませんでした".into()));
        assert!(matches!(panel.eval_result(), Some(EvalOutcome::Failed(_))));
        assert!(!panel.eval_running());
        // 採用は従来どおり通る
        let msg = panel.adopt(0).expect("評価の失敗は採用を妨げない");
        assert!(
            msg.contains("fast-forward") || msg.contains("マージ"),
            "msg={msg}"
        );
        assert_eq!(
            panel.race.as_ref().unwrap().racers[0].status,
            RacerStatus::Adopted
        );
        // 破棄も従来どおり通る
        panel.discard(1, true).expect("破棄も従来どおり");
        let snapshot = panel.race.as_ref().unwrap().clone();
        cleanup(&snapshot);
    }

    #[test]
    fn 評価は自動では走らない() {
        // 設計原則 3: アイドル時のコストはゼロ。レースを始めただけでは評価しない。
        let Some(repo) = fixture_repo("evalidle") else {
            return;
        };
        let race = start_race(&repo, "idle", &two_presets()).expect("開始");
        let mut panel = RacePanel::new();
        panel.begin(race);
        assert!(panel.eval_result().is_none(), "始めただけでは評価しない");
        assert!(!panel.eval_running());
        let snapshot = panel.race.as_ref().unwrap().clone();
        cleanup(&snapshot);
    }
}
