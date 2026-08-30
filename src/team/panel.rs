//! Team 画面の状態と、GUI から Runtime を動かす橋。
//!
//! ## なぜ `ZaivernApp` のフィールドにしないのか
//!
//! `ZaivernApp` の構造体と初期化は**全ブランチが取り合う共有面**で、欄を 1 つ
//! 足すだけで並列の機能ブランチが同時に壊れる (CLAUDE.md の実測)。Team の
//! 状態は UI スレッドからしか触らないので、ここに `thread_local!` で持つと
//! 共有面を 1 バイトも増やさずに済む。
//!
//! 設計原則 1 (「ターミナルのモデルはウィンドウより長生きさせる」) とも
//! 揃う — Runtime は UI の破棄を生き延びる場所に居て、UI はその純粋な
//! ビューになる。
//!
//! ## 描画中に副作用を起こさない
//!
//! [`TeamPanel::pump`] は**描画の外**で呼ぶ。描画側 ([`super::organization_board`])
//! は [`TeamSnapshot`](super::view_model::TeamSnapshot) を読んで
//! [`BoardAction`] を返すだけで、プロセス起動もファイル書き込みもしない。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::model::*;
use super::persistence::{self, LoadOutcome};
use super::planner::{PlanInput, StaticPlanner, TeamPlanner};
use super::runtime::{Observation, RunOptions, RunOwner, TeamAction, TeamEffect, TeamRuntime};
use super::view_model::{self, TeamSnapshot};

/// 画面を走査する間隔。**毎フレームは舐めない** (UI スレッドで走るので、
/// 64 体ぶんの画面を 60fps で解析すると確実にフレームが落ちる)。
pub const SCAN_INTERVAL: Duration = Duration::from_millis(400);

/// 起動要求 (`zai team run` の投函) を見に行く間隔。
///
/// **毎フレーム `stat` を撃たない。** 画面が動いている間は 60fps で
/// 呼ばれるので、1 フレームに 1 回のシステムコールでも積み上がる
/// (設計原則 3: アイドル時のコストはゼロ)。人が `zai team run` を打って
/// から 1 秒以内に反応すれば、待たされたとは感じない。
pub const LAUNCH_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// セッションごとに覚えておく「もう読んだ行」の数。
pub const SEEN_LINES_CAP: usize = 600;

/// 1 回の走査で読む画面の行数。
pub const SCAN_ROWS: usize = 200;
/// 1 行あたりの文字数上限。
pub const SCAN_COLS: usize = 400;

/// app 側から渡す 1 セッションの観測材料。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInput {
    pub id: SessionId,
    pub title: String,
    pub provider: String,
    pub state: crate::coordinator::SessionState,
    /// 画面末尾のテキスト (行ごと)。**全履歴を渡さないこと。**
    pub tail: Vec<String>,
}

/// Team 画面のタブ。**同時に 2 つは描かない** (中央ビューと同じ理由で
/// 独立した bool を持たない)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BoardTab {
    #[default]
    Organization,
    Tasks,
    Terminals,
    Timeline,
}

impl BoardTab {
    pub fn key(self) -> &'static str {
        match self {
            BoardTab::Organization => "organization",
            BoardTab::Tasks => "tasks",
            BoardTab::Terminals => "terminals",
            BoardTab::Timeline => "timeline",
        }
    }
    pub const ALL: [BoardTab; 4] = [
        BoardTab::Organization,
        BoardTab::Tasks,
        BoardTab::Terminals,
        BoardTab::Timeline,
    ];
}

/// 画面が Runtime に返す要求。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoardAction {
    Close,
    SwitchTab(BoardTab),
    Start,
    Pause,
    Resume,
    Stop,
    Approve(EventId),
    Reject(EventId),
    Retry(TaskId),
    Reassign(TaskId),
    /// エージェントを選ぶ (Inspector を開く)。
    Select(AgentId),
    /// タスクへ追加の指示を足す (Inspector の Edit Instruction)。
    AddContext { task: TaskId, text: String },
    SelectTask(TaskId),
    /// 実際の端末を開く。**`ManagedSession` のときだけ**。
    OpenTerminal(SessionId),
    /// New Team Run のフォームを開く。
    OpenNewRun,
    /// フォームの内容で計画する。
    PlanFromForm,
    /// 未完了 Run の扱い。
    ResumeRun,
    DiscardRun,
    OpenReadOnly,
}

/// New Team Run フォームの入力。
#[derive(Clone, Debug, PartialEq)]
pub struct NewRunForm {
    pub open: bool,
    pub goal_name: String,
    pub spec_path: String,
    pub spec_text: String,
    /// SPEC をファイルから読むか、直接入力か。
    pub from_file: bool,
    pub agents: usize,
    pub max_attempts: u8,
    pub review_required: bool,
    /// エージェントプリセット — チームに置く役割。
    ///
    /// **既定は実装 + レビューの 2 つ。** 役割を増やすほど 1 体あたりの
    /// 仕事は減るので、最大同時数を超える役割は選べない。
    pub roles: Vec<TeamRole>,
    /// 承認モード (`ask` / `auto` / `agent`)。既存の承認モードと同じ綴り。
    pub approval_mode: String,
    /// コスト上限 (USD)。0 なら上限なし。
    pub cost_limit: f32,
    /// 直近のエラー文面。
    pub error: String,
}

impl Default for NewRunForm {
    fn default() -> Self {
        Self {
            open: false,
            goal_name: String::new(),
            spec_path: "SPEC.md".to_string(),
            spec_text: String::new(),
            from_file: true,
            // 仕様の初期値
            agents: 4,
            max_attempts: 3,
            review_required: true,
            roles: vec![TeamRole::Implementer, TeamRole::Reviewer],
            approval_mode: "ask".to_string(),
            cost_limit: 0.0,
            error: String::new(),
        }
    }
}

/// 未完了 Run が見つかったときの選択。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RestorePrompt {
    #[default]
    None,
    /// 未完了 Run がある。Resume / Open Read Only / Discard を出す。
    Found,
    /// Discard の確認中。
    ConfirmDiscard,
}

/// 裏で走っている検証 1 件。**受け口と一緒に「何の実行か」を持つ。**
///
/// 送り手が消えたときに失敗として戻すには、タスク・実行 ID・コマンドが
/// 要る。受け口だけを配列で持っていた頃は、切断を握り潰す以外に手が無く、
/// `validation.running` が true のまま残っていた。
/// 実行器の時限を過ぎてから、こちら側が見切るまでの余白 (秒)。
pub const WATCHDOG_SLACK_SECS: u64 = 60;

pub struct ValidationJob {
    /// **持ち主。** 戻ってきた結果を、いまの Run のものだけに限る。
    ///
    /// 実行 ID にも `run_id` が入っているので二重の守りだが、外向きの
    /// 4 つの口と同じ形にしておく — 片方だけ文字列の一致に頼っていると、
    /// 書式を変えた日に静かに守りが消える。
    pub owner: RunOwner,
    pub task: TaskId,
    /// `run_id:task:attempt:generation`。結果の突き合わせに使う。
    pub execution: String,
    /// 走らせているコマンド (切断時に失敗として戻すため)。
    pub commands: Vec<String>,
    /// 開始時刻 (Unix 秒)。見張りの基準になる。
    pub started_at: u64,
    /// 実行器へ渡した時間切れ (秒)。
    ///
    /// 実行器が自分で打ち切れなかったとき (worker がブロックしたまま何も
    /// 送らない) の**最後の砦**。これを持たないと「切断も来ない・結果も
    /// 来ない」経路だけが永久に残る。
    pub timeout_secs: u64,
    /// 停止の札。立てると実行器がプロセスツリーごと終了させる。
    pub cancel: super::launch::CancelFlag,
    /// いま走っている子の PID (0 = 走っていない)。
    ///
    /// **札だけでは足りない場面がある。** アプリを閉じるときは worker ごと
    /// 消えるので、誰も木を落とさない (子は自分のプロセスグループを持つので
    /// 親と一緒には死なない)。閉じる側がここを見て自分で落とす。
    pub pid: super::launch::PidSlot,
    /// 結果の受け口 (実行 ID, タスク, 実測)。
    pub rx: std::sync::mpsc::Receiver<(String, TaskId, Vec<ValidationRun>)>,
}

/// いま面倒を見ているものの数 (workspace を切り替えてよいかの判断)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LiveWork {
    /// セッションが結び付いているエージェント。
    pub agents: usize,
    /// 走っている検証。
    pub validations: usize,
    /// まだ実行していない Effect。
    pub effects: usize,
}

impl LiveWork {
    /// 放置すると孤児になるものがあるか。
    pub fn is_busy(&self) -> bool {
        self.agents > 0 || self.validations > 0 || self.effects > 0
    }

    /// 断る理由 (そのまま画面に出す)。
    pub fn why_blocked(&self) -> String {
        crate::i18n::trf(
            "team.err.workspace_busy",
            &[
                ("agents", self.agents.to_string()),
                ("validations", self.validations.to_string()),
            ],
        )
    }
}

/// Team 画面の状態。
pub struct TeamPanel {
    pub open: bool,
    pub tab: BoardTab,
    pub form: NewRunForm,
    pub selected_agent: Option<AgentId>,
    pub selected_task: Option<TaskId>,
    pub inspector_open: bool,
    /// Inspector の「追加の指示」入力欄。
    pub inspector_note: String,
    pub restore: RestorePrompt,
    /// 読み取り専用で開いているか (復元して眺めるだけ)。
    pub read_only: bool,
    /// 直近の説明・エラー (画面の帯に出す)。
    pub notice: String,
    runtime: Option<TeamRuntime>,
    snapshot: Option<TeamSnapshot>,
    /// スナップショットを作り直す必要があるか。
    /// **60fps で全件を作り直さない**ための印。
    dirty: bool,
    /// 実行側へ渡す仕事。**冪等キーを必ず添える** — 実行できたら
    /// [`TeamRuntime::note_effect_done`] へ、失敗したら
    /// [`TeamRuntime::note_effect_failed`] へ返すため。返さない限り
    /// Runtime は「済んだ」と見なさないので、途中で落ちても失われない。
    /// **持ち主 (`RunOwner`) を必ず添える。** 実行の直前にもう一度
    /// 突き合わせ、いまの Run のものでなければ実行しない — workspace を
    /// 切り替えた瞬間に前の Run の仕事が新しいフォルダで動くのを、
    /// キューを空にする偶然ではなく**構造で**防ぐ。
    pending_launches: Vec<(RunOwner, String, super::runtime::AgentLaunchSpec)>,
    /// 実行を頼んだ検証。
    pending_validations: Vec<(RunOwner, String, super::runtime::ValidationSpec)>,
    /// 停止を頼んだセッション。
    pending_stops: Vec<(RunOwner, String, SessionId)>,
    /// 送るべき指示 (持ち主, 冪等キー, セッション ID, 本文)。
    pending_instructions: Vec<(RunOwner, String, (TaskId, SessionId, String))>,
    /// 別の Run のものとして捨てた Effect の数 (診断とテストが見る)。
    dropped_effects: usize,
    /// **テスト専用**: 「実行中だから断る」を黙らせる。
    ///
    /// 断りの有無と**別に**、持ち主の照合が効くことを確かめるため。
    #[cfg(test)]
    bypass_busy: bool,
    /// 保存が要るか。
    needs_save: bool,
    workspace: PathBuf,
    /// 状態の置き場の**根**。既定は `~/.zaivern`。
    ///
    /// テストはここを一時ディレクトリへ向ける (`ZAIVERN_HOME` を差し替えると
    /// 並列に走る他のテストへ漏れるので使わない)。
    home: PathBuf,
    /// セッションごとの「もう読んだ行」(順序つき・上限あり)。
    seen_order: HashMap<SessionId, VecDeque<u64>>,
    seen_set: HashMap<SessionId, HashSet<u64>>,
    /// 次に走査してよい時刻。**`Instant` は永続化しない。**
    next_scan: Option<Instant>,
    /// 次に起動要求を見に行ってよい時刻。
    next_launch_poll: Option<Instant>,
    /// 裏で走らせている検証の受け口。
    ///
    /// **UI スレッドでブロッキング I/O をしない**ので、`try_recv` で
    /// 拾える形にして持つ。
    /// 走らせている検証。**受け口だけを持たない。**
    ///
    /// 送り手が消えたとき (worker の panic) に、どのタスクの・どの実行の・
    /// 何のコマンドが失われたのかが分からないと、`validation.running` を
    /// 下ろすことも失敗として記録することもできない。
    validation_jobs: Vec<ValidationJob>,
}

impl Default for TeamPanel {
    fn default() -> Self {
        Self {
            open: false,
            tab: BoardTab::default(),
            form: NewRunForm::default(),
            selected_agent: None,
            selected_task: None,
            inspector_open: false,
            inspector_note: String::new(),
            restore: RestorePrompt::None,
            read_only: false,
            notice: String::new(),
            runtime: None,
            snapshot: None,
            dirty: true,
            pending_launches: Vec::new(),
            pending_validations: Vec::new(),
            pending_stops: Vec::new(),
            pending_instructions: Vec::new(),
            needs_save: false,
            workspace: PathBuf::new(),
            home: persistence::default_home(),
            seen_order: HashMap::new(),
            seen_set: HashMap::new(),
            next_scan: None,
            next_launch_poll: None,
            validation_jobs: Vec::new(),
            dropped_effects: 0,
            #[cfg(test)]
            bypass_busy: false,
        }
    }
}

impl TeamPanel {
    pub fn has_run(&self) -> bool {
        self.runtime.is_some()
    }

    /// いまの Goal の状態 (画面と操作の判断に使う唯一の入口)。
    pub fn goal_status(&self) -> Option<GoalStatus> {
        self.runtime.as_ref().map(|r| r.goal().status)
    }

    /// 画面が読むスナップショット。無ければ `None`。
    pub fn snapshot(&self) -> Option<&TeamSnapshot> {
        self.snapshot.as_ref()
    }

    /// 状態の置き場 (`<根>/team/<ワークスペースキー>/`)。
    /// いま面倒を見ている workspace。**計画も SPEC の解決もここを基準にする。**
    ///
    /// 画面の「いまのフォルダ」(`agent_cwd`) と食い違うことがある — 実行中の
    /// Run があると切り替えを断るため。基準を 2 つ持つと、Run の workspace と
    /// 違う場所の SPEC を読む・違う場所でエージェントを起こす、が起きる。
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn state_dir(&self) -> PathBuf {
        persistence::team_dir_in(&self.home, &self.workspace)
    }

    /// **いま面倒を見ているものがあるか。**
    ///
    /// あるうちは workspace を切り替えない — 切り替えると Runtime への参照が
    /// 消えて、**画面から消えたのに裏で動き続けるエージェントと検証**が残る
    /// (誰も結果を受け取らず、誰も止められない)。
    ///
    /// **`runtime.is_some()` では広すぎる。** 計画しただけ・停止し終えた
    /// Run で永久に切り替え不能になる。見るのは「放置すると孤児になるもの」:
    ///
    /// * セッションが結び付いているエージェント
    /// * 走っている検証
    /// * まだ実行していない Effect
    pub fn live_work(&self) -> LiveWork {
        LiveWork {
            // エージェントは Runtime が知っている。
            agents: self
                .runtime
                .as_ref()
                .map(|rt| {
                    rt.agents()
                        .iter()
                        .filter(|a| a.kind == AgentKind::ManagedSession && a.session_id.is_some())
                        .count()
                })
                .unwrap_or(0),
            // **検証と未実行の仕事は画面側の持ち物。** Runtime が無い
            // ときに 0 と答えると、`discard_run` の直後 (Runtime は捨てた
            // が子プロセスはまだ畳んでいる) に「空いている」と嘘をつく。
            validations: self.validation_jobs.len(),
            effects: self.pending_launches.len()
                + self.pending_instructions.len()
                + self.pending_stops.len()
                + self.pending_validations.len(),
        }
    }

    /// ワークスペースを設定し、保存済みの Run があれば知らせる。
    ///
    /// **実行中のものがあれば切り替えない。** 断った理由をそのまま返す
    /// (黙って無視すると、利用者は「押したのに何も起きない」を見る)。
    pub fn attach_workspace(&mut self, ws: &Path) -> Result<(), String> {
        if self.workspace == ws {
            return Ok(());
        }
        let live = self.live_work();
        if live.is_busy() {
            let why = live.why_blocked();
            self.notice = why.clone();
            return Err(why);
        }
        // ここへ来るのは「面倒を見ているものが無い」ときだけ。それでも
        // **捨てる前に後始末をする** — 走り出しかけた検証と、書き残しの
        // 保存を置き去りにしない。
        self.cancel_all_validations();
        self.validation_jobs.clear();
        self.pending_launches.clear();
        self.pending_instructions.clear();
        self.pending_stops.clear();
        self.pending_validations.clear();
        self.save_if_needed();
        self.workspace = ws.to_path_buf();
        self.runtime = None;
        self.snapshot = None;
        self.read_only = false;
        self.restore = if persistence::has_run(&self.state_dir()) {
            RestorePrompt::Found
        } else {
            RestorePrompt::None
        };
        Ok(())
    }

    /// **いまの設定をフォームの初期値にする** (既存設定は書き換えない)。
    ///
    /// フォームの既定は `ask` / `0` で、`0` は「上限なし」を意味する。
    /// 読まずに計画へ流すと、`agent` / `25` で使っている人の環境で
    /// フォームを開いただけで承認モードが下がり、上限が外れる。
    ///
    /// **開いている間は上書きしない。** 人が選び直した値を、次のフレームの
    /// 読み込みで元へ戻してしまう。
    pub fn seed_guardrails(&mut self, approval_mode: &str, cost_limit: f32) {
        if self.form.open {
            return;
        }
        self.form.approval_mode = approval_mode.to_string();
        self.form.cost_limit = cost_limit.max(0.0);
    }

    /// いまの Run にだけ効く締め具合 (Run が無ければ `None`)。
    pub fn run_guardrails(&self) -> Option<RunGuardrails> {
        self.runtime.as_ref().map(|rt| rt.run().guardrails.clone())
    }

    /// 計画を作って Runtime を立てる (まだ開始はしない)。
    ///
    /// `roles` はフォームの「エージェントプリセット」、`title_override` は
    /// 「Goal 名」。**どちらも実際に計画へ効く** — 選べるのに何も変わらない
    /// 入力欄を残さない。
    pub fn plan_with(
        &mut self,
        spec_text: &str,
        source: &str,
        opts: RunOptions,
        roles: Vec<TeamRole>,
        title_override: &str,
    ) -> Result<(), String> {
        let plan = StaticPlanner
            .plan(PlanInput {
                spec: spec_text.to_string(),
                source: source.to_string(),
                agent_count: opts.agent_count,
                review_required: opts.review_required,
                workspace_root: self.workspace.clone(),
                roles,
            })
            .map_err(|e| e.detail())?;
        // **配る前に計画そのものを検証する。**
        let issues = super::graph::validate_plan(&plan.tasks, &plan.goal.definition_of_done);
        if !issues.is_empty() {
            return Err(issues
                .iter()
                .map(|i| i.detail())
                .collect::<Vec<_>>()
                .join("\n"));
        }
        // **動いているチームを別の計画で潰さない。** 置き換えると、走って
        // いる検証と起動済みのエージェントの面倒を見る相手が消える
        // (結果は `run_id` 違いで捨てられ、プロセスだけが残る)。
        self.refuse_if_busy()?;
        let ws = self.workspace.clone();
        let mut rt = TeamRuntime::from_plan(plan, ws, opts);
        let t = title_override.trim();
        if !t.is_empty() {
            rt.rename_goal(t);
        }
        self.runtime = Some(rt);
        self.read_only = false;
        self.restore = RestorePrompt::None;
        self.dirty = true;
        self.needs_save = true;
        Ok(())
    }

    /// 面倒を見ているものがあるなら断る (理由をそのまま返す)。
    fn refuse_if_busy(&mut self) -> Result<(), String> {
        #[cfg(test)]
        if self.bypass_busy {
            return Ok(());
        }
        let live = self.live_work();
        if !live.is_busy() {
            return Ok(());
        }
        let why = live.why_blocked();
        self.notice = why.clone();
        Err(why)
    }

    /// **テスト専用**: 実行中でも Run を作り直せるようにする。
    ///
    /// 「前の Run の仕事が新しい Run で実行されない」ことは、**断りの有無と
    /// 別に**成り立っていなければならない。断りを外しても持ち主の照合が
    /// 効くことを確かめられるように、検査だけを黙らせる口を残す
    /// (Runtime を建てるのは `plan_with` の 1 か所のまま)。
    #[cfg(test)]
    pub fn allow_replace_run_for_test(&mut self) {
        self.bypass_busy = true;
    }

    /// 既定のプリセットで計画する (CLI 経由と、表題を SPEC から起こす場合)。
    pub fn plan(&mut self, spec_text: &str, source: &str, opts: RunOptions) -> Result<(), String> {
        self.plan_with(spec_text, source, opts, Vec::new(), "")
    }

    /// 保存された Run を復元する。
    pub fn restore_run(&mut self, read_only: bool) -> Result<(), String> {
        let dir = self.state_dir();
        match persistence::load(&dir) {
            LoadOutcome::Loaded(s) => {
                self.runtime = Some(TeamRuntime::restore(*s, self.workspace.clone()));
                self.read_only = read_only;
                self.restore = RestorePrompt::None;
                self.dirty = true;
                Ok(())
            }
            LoadOutcome::Empty => Err("保存された Team Run がありません".to_string()),
            LoadOutcome::Corrupt { backed_up, reason } => Err(format!(
                "{reason}\n退避しました: {}",
                backed_up.join(", ")
            )),
            LoadOutcome::Newer { found } => Err(format!(
                "保存された状態の版 ({found}) が新しすぎます。Zaivern を更新してください。"
            )),
        }
    }

    /// 保存された Run を消す (**確認済みの呼び出しだけ**)。
    ///
    /// **走っている検証を置き去りにしない。** 記録だけ消して `cargo test` が
    /// 走り続けると、誰も結果を受け取らないプロセスがリポジトリを触り続ける。
    pub fn discard_run(&mut self) -> Result<usize, String> {
        self.cancel_all_validations();
        let dir = self.state_dir();
        let n = persistence::reset(&dir).map_err(|e| e.detail())?;
        self.runtime = None;
        self.snapshot = None;
        self.restore = RestorePrompt::None;
        Ok(n)
    }

    /// 人の操作を Runtime へ渡す。
    pub fn act(&mut self, action: TeamAction) {
        if self.read_only {
            self.notice = "読み取り専用で開いています (操作できません)".to_string();
            return;
        }
        let Some(rt) = self.runtime.as_mut() else {
            return;
        };
        let effects = rt.apply_action(action);
        self.absorb(effects);
        self.dirty = true;
    }

/// 走査してよい時刻か。**毎フレームは走らせない。**
    pub fn scan_due(&mut self, now: Instant) -> bool {
        match self.next_scan {
            Some(t) if now < t => false,
            _ => {
                self.next_scan = Some(now + SCAN_INTERVAL);
                true
            }
        }
    }

    /// 起動要求を見に行ってよい時刻か。**毎フレームは撃たない。**
    pub fn launch_poll_due(&mut self, now: Instant) -> bool {
        match self.next_launch_poll {
            Some(t) if now < t => false,
            _ => {
                self.next_launch_poll = Some(now + LAUNCH_POLL_INTERVAL);
                true
            }
        }
    }

    /// セッションが消えたら、その記憶も捨てる (無制限に溜めない)。
    pub fn forget_session(&mut self, id: SessionId) {
        self.seen_order.remove(&id);
        self.seen_set.remove(&id);
    }

    /// **前回以降に増えた行だけ**を取り出す。
    ///
    /// 画面は同じ行を何度も映すので、これが無いと 1 通の完了報告が毎回
    /// 読み直される (却下が何十件も並ぶ)。
    fn new_lines(&mut self, id: SessionId, tail: &[String]) -> String {
        let mut out = String::new();
        for line in tail {
            let h = fnv1a(line.as_bytes());
            let set = self.seen_set.entry(id).or_default();
            if !set.insert(h) {
                continue;
            }
            let order = self.seen_order.entry(id).or_default();
            order.push_back(h);
            while order.len() > SEEN_LINES_CAP {
                if let Some(old) = order.pop_front() {
                    self.seen_set.entry(id).or_default().remove(&old);
                }
            }
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    /// app から観測を受け取って 1 tick 進める。**描画の外で呼ぶこと。**
    pub fn pump_sessions(&mut self, rows: Vec<SessionInput>, now: u64) {
        if self.runtime.is_none() || self.read_only {
            return;
        }
        let live: HashSet<SessionId> = rows.iter().map(|r| r.id).collect();
        let gone: Vec<SessionId> = self
            .seen_set
            .keys()
            .copied()
            .filter(|id| !live.contains(id))
            .collect();
        for id in gone {
            self.forget_session(id);
        }
        let sessions = rows
            .into_iter()
            .map(|r| {
                let text = self.new_lines(r.id, &r.tail);
                super::runtime::SessionObs {
                    id: r.id,
                    title: r.title,
                    provider: r.provider,
                    state: r.state,
                    text,
                }
            })
            .collect();
        self.pump(Observation { now, sessions });
    }

    /// 1 tick 進める。**描画の外で呼ぶこと。**
    pub fn pump(&mut self, obs: Observation) {
        if self.read_only {
            return;
        }
        let Some(rt) = self.runtime.as_mut() else {
            return;
        };
        let effects = rt.tick(&obs);
        self.absorb(effects);
    }

    fn absorb(&mut self, effects: Vec<TeamEffect>) {
        // **発行した瞬間の持ち主を焼き付ける。** あとから「いまの Runtime」を
        // 見て決めると、切り替わった後の値になってしまう。
        let Some(owner) = self.runtime.as_ref().map(|rt| rt.owner()) else {
            return;
        };
        for e in effects {
            let key = e.key();
            match e {
                TeamEffect::StartAgent(s) => {
                    self.pending_launches.push((owner.clone(), key, s))
                }
                TeamEffect::SendInstruction {
                    task, session, text, ..
                } => self
                    .pending_instructions
                    .push((owner.clone(), key, (task, session, text))),
                TeamEffect::StopAgent(s) => self.pending_stops.push((owner.clone(), key, s)),
                TeamEffect::RunValidation(v) => {
                    self.pending_validations.push((owner.clone(), key, v))
                }
                TeamEffect::RequestHumanApproval(_) => {
                    // 判断は Runtime が保持していて、画面が Mission Panel で出す。
                    // ここで別の入れ物へ写すと第 2 の真実になる。
                    // **画面に出た時点で仕事は済んでいる**ので、そのまま成功を返す。
                    self.ack_done(&key);
                }
                TeamEffect::CancelValidation {
                    execution, task, ..
                } => {
                    // 相手が居なくても目的は果たされている (走っていない)。
                    let _ = (self.cancel_validation(&execution), task);
                    self.ack_done(&key);
                }
                TeamEffect::PersistState => self.needs_save = true,
            }
            self.dirty = true;
        }
    }

    /// Effect の実行が成功したと Runtime へ返す。
    pub fn ack_done(&mut self, key: &str) {
        if let Some(rt) = self.runtime.as_mut() {
            rt.note_effect_done(key);
        }
        self.needs_save = true;
        self.dirty = true;
    }

    /// **指示が方針で止められた**と Runtime へ返す (撃ち直さない)。
    pub fn note_instruction_blocked(&mut self, task: TaskId, why: &str) {
        if let Some(rt) = self.runtime.as_mut() {
            rt.note_instruction_blocked(task, why);
        }
        self.needs_save = true;
        self.dirty = true;
    }

    /// Effect の実行に失敗したと Runtime へ返す (次の tick で再発行される)。
    pub fn ack_failed(&mut self, key: &str) {
        if let Some(rt) = self.runtime.as_mut() {
            rt.note_effect_failed(key);
        }
        self.needs_save = true;
        self.dirty = true;
    }

    /// いまの Run の持ち主。Run が無ければ `None`。
    pub fn owner(&self) -> Option<RunOwner> {
        self.runtime.as_ref().map(|rt| rt.owner())
    }

    /// **いまの Run のものだけを渡す。** 持ち主が違うものは捨てる
    /// (別の workspace / 別の Run で実行させない)。捨てた件数を数える。
    fn mine<T>(&mut self, q: Vec<(RunOwner, String, T)>) -> Vec<(String, T)> {
        let now = self.owner();
        let mut out = Vec::with_capacity(q.len());
        let mut dropped = 0usize;
        for (owner, key, v) in q {
            if now.as_ref() == Some(&owner) {
                out.push((key, v));
            } else {
                dropped += 1;
            }
        }
        if dropped > 0 {
            // **黙って捨てない。** 画面には「前の Run の仕事を実行しなかった」
            // ことが出る (何も起きないまま消えると、利用者は理由を追えない)。
            self.dropped_effects = self.dropped_effects.saturating_add(dropped);
            self.notice = crate::i18n::trf(
                "team.notice.dropped_effects",
                &[("n", dropped.to_string())],
            );
        }
        out
    }

    /// 別の Run のものとして捨てた Effect の数。
    ///
    /// 画面へは `notice` で出る (上の `mine`)。この数を読むのはテストだけ。
    #[cfg(test)]
    pub fn dropped_effects(&self) -> usize {
        self.dropped_effects
    }

    /// 起動してほしいエージェント (冪等キー付き。取り出したら消える)。
    pub fn take_launches(&mut self) -> Vec<(String, super::runtime::AgentLaunchSpec)> {
        let q = std::mem::take(&mut self.pending_launches);
        self.mine(q)
    }
    /// 送ってほしい指示 (冪等キー付き。取り出したら消える)。
    ///
    /// **宛先のタスクも一緒に渡す。** 実行側がセッションから引き直すと、
    /// 間に 1 tick 入っただけで別のタスクを指す。
    pub fn take_instructions(&mut self) -> Vec<(String, TaskId, SessionId, String)> {
        let q = std::mem::take(&mut self.pending_instructions);
        self.mine(q)
            .into_iter()
            .map(|(k, (task, s, t))| (k, task, s, t))
            .collect()
    }
    /// 止めてほしいセッション (冪等キー付き。取り出したら消える)。
    pub fn take_stops(&mut self) -> Vec<(String, SessionId)> {
        let q = std::mem::take(&mut self.pending_stops);
        self.mine(q)
    }
    /// 走らせてほしい検証 (冪等キー付き。取り出したら消える)。
    pub fn take_validations(&mut self) -> Vec<(String, super::runtime::ValidationSpec)> {
        let q = std::mem::take(&mut self.pending_validations);
        self.mine(q)
    }

    /// 起動したセッションを結び付ける。
    ///
    /// `identity` は**再起動をまたぐ目印** (実行側が決める安定した文字列)。
    /// 覚えておかないと、次の起動で同じ logical agent を 2 体起こす。
    pub fn bind_session(
        &mut self,
        agent: &AgentId,
        session: SessionId,
        identity: Option<String>,
    ) {
        if let Some(rt) = self.runtime.as_mut() {
            rt.bind_session(agent, session, identity);
        }
        self.dirty = true;
        self.needs_save = true;
    }

    /// **指示が実際に届いたか**を実行側から受け取る (`submit` の終わり方)。
    ///
    /// 届いたなら冪等キーを完了にする (もう出し直さない)。届かなかったなら
    /// 完了にせず、担当を解いて配り直せる形へ戻す。**積めた時点で完了に
    /// してしまうと、相手が消えた場合に指示が消えたまま Runtime だけが
    /// 「送った」と信じ続ける。**
    ///
    /// 戻りは「届かなかったタスク」(画面へ理由を出すため)。
    pub fn note_delivery(&mut self, tag: &str, delivered: bool) -> Option<TaskId> {
        let (run, key) = tag.split_once('|')?;
        // **前の Run の配達を、いまの Run へ効かせない。** 積んだ仕事は
        // Run の切り替えでは消えないので、同じ番号の別のタスクを完了に
        // してしまう (外向きの 4 つの口と同じ「持ち主で選り分ける」形)。
        if self.owner().is_none_or(|o| o.run_id != run) {
            return None;
        }
        let key = key.to_string();
        let key = key.as_str();
        let task = self.instruction_task_of(key)?;
        if delivered {
            self.ack_done(key);
            return None;
        }
        self.ack_failed(key);
        if let Some(rt) = self.runtime.as_mut() {
            rt.note_instruction_undelivered(task, key, "宛先の端末が応答しませんでした");
        }
        self.needs_save = true;
        self.dirty = true;
        Some(task)
    }

    /// 配達の結末を受け取るための目印 (`<run_id>|<冪等キー>`)。
    ///
    /// Run を添えるのは、積んだ仕事が Run の切り替えでは消えないから
    /// (前の Run の配達が、同じ番号の別のタスクを完了にしてしまう)。
    /// Run が無ければ `None` = そもそも配達の結末を受け取らない。
    pub fn delivery_tag(&self, key: &str) -> Option<String> {
        self.owner().map(|o| format!("{}|{key}", o.run_id))
    }

    /// 冪等キーからタスク番号を読む (`instr:<task>:<agent>:<attempt>:<seq>`)。
    ///
    /// **鍵の綴りを 2 か所で組み立てない。** 発行側は
    /// [`super::runtime`] の 1 か所だけで、ここは読むだけ。読めない綴り
    /// (Team のものではない目印) は `None` になり、何も起きない。
    fn instruction_task_of(&self, key: &str) -> Option<TaskId> {
        let rest = key.strip_prefix("instr:")?;
        let (task, _) = rest.split_once(':')?;
        task.parse().ok()
    }

    /// いま担当へ結び付いているセッション。
    ///
    /// 実行側が「このセッションはもう別の担当のものか」を見るために使う
    /// (**第 2 のセッション台帳を作らない** — 真実は Runtime の
    /// エージェント一覧 1 か所)。
    pub fn bound_sessions(&self) -> HashSet<SessionId> {
        self.runtime
            .as_ref()
            .map(|rt| rt.agents().iter().filter_map(|a| a.session_id).collect())
            .unwrap_or_default()
    }

    /// 起動に失敗した。
    pub fn note_launch_failed(&mut self, agent: &AgentId, why: &str) {
        if let Some(rt) = self.runtime.as_mut() {
            rt.note_launch_failed(agent, why);
        }
        self.dirty = true;
    }

    /// 検証結果を戻す。
    /// **実行 ID を添えて**実測を戻す (古い実行の結果を採らないため)。
    pub fn note_validation_for(
        &mut self,
        execution: &str,
        task: TaskId,
        runs: Vec<ValidationRun>,
    ) {
        if let Some(rt) = self.runtime.as_mut() {
            rt.note_validation_for(execution, task, runs);
        }
        self.needs_save = true;
        self.dirty = true;
    }

    /// 裏で走らせた検証を預かる。
    pub fn watch_validation(&mut self, job: ValidationJob) {
        self.validation_jobs.push(job);
    }

    /// 走っている検証の数 (UI とテストが見る)。
    pub fn running_validations(&self) -> usize {
        self.validation_jobs.len()
    }

    /// 指定した実行を止めるよう札を立てる。**プロセスは実行器が落とす。**
    ///
    /// 戻り値は「止める相手が居たか」。世代がずれていれば既に別の実行なので
    /// 何もしない (古い停止要求で新しい検証を殺さない)。
    pub fn cancel_validation(&mut self, execution: &str) -> bool {
        let mut hit = false;
        for j in &self.validation_jobs {
            if j.execution == execution {
                j.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                hit = true;
            }
        }
        hit
    }

    /// 走っている検証を全部止める (Run を閉じる / 破棄するとき)。戻りは件数。
    ///
    /// **札を立てるだけ。** worker が次の刻みで木を落として `Cancelled` を
    /// 戻すので、結果を受け取る口はそのまま残す (捨てると Runtime が
    /// 永久に待つ)。アプリを閉じるときだけは待てないので
    /// [`Self::stop_all_validations_now`] を使う。
    pub fn cancel_all_validations(&mut self) -> usize {
        for j in &self.validation_jobs {
            j.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.running_validations()
    }

    /// **アプリを閉じるときの後始末。**
    ///
    /// この状態は `thread_local!` に居るので、`ZaivernApp` より長生きする。
    /// 生き残った Runtime を次のアプリが拾うと、**もう居ないセッションへ
    /// 結び付いたまま**の状態を新しい画面が見ることになる。保存してから
    /// 手放し、次回は保存経路 (`restore`) から入り直す — そこで結び付きは
    /// 必ず外れる。走っている検証はその場で落とす (札だけでは死なない)。
    pub fn shutdown(&mut self) -> usize {
        let killed = self.stop_all_validations_now();
        self.pending_launches.clear();
        self.pending_instructions.clear();
        self.pending_stops.clear();
        self.pending_validations.clear();
        self.save_if_needed();
        self.runtime = None;
        self.snapshot = None;
        self.restore = RestorePrompt::None;
        killed
    }

    /// **新しいアプリ文脈がこのスレッドに現れた**ときの引き継ぎ拒否。
    ///
    /// 状態は `thread_local!` に居るので `ZaivernApp` より長生きする。
    /// [`Self::shutdown`] は終わる側が呼ぶが、**呼ばれないまま次のアプリが
    /// 立つ経路がある** (前のアプリが落ちた / 同じスレッドで 2 つ目が
    /// 立った)。そのとき生き残った Runtime をそのまま拾うと、新しい画面が
    /// **もう自分のものではないセッションへ結び付いた Run** を操作できて
    /// しまう (起動済みエージェントも走っている検証も、前のアプリのもの)。
    ///
    /// なので**暗黙の引き継ぎを構造で断つ**: 閉じるときと同じ後始末をして
    /// 手放し、`workspace` も空へ戻す。新しいアプリは必ず
    /// `attach_workspace` → 保存経路 (`restore`) から入り直すので、
    /// 復元した Run はそのアプリのセッションへ結び直される。
    ///
    /// 戻りは「前のアプリの状態が残っていたか」。**普通の起動では false**
    /// (何も無いところから始まるので、1 命令も走らない)。
    pub fn adopt_new_app_context(&mut self) -> bool {
        if self.runtime.is_none()
            && self.workspace.as_os_str().is_empty()
            && self.validation_jobs.is_empty()
        {
            return false;
        }
        // 走っている検証は落とす。**渡さないなら面倒を見る相手が居ない**
        // ので、札を立てるだけでは孤児のプロセスが残る。
        self.shutdown();
        // **`workspace` も空へ戻す。** 残すと、同じフォルダを開いた次の
        // アプリの `attach_workspace` が「同じだから何もしない」で早々に
        // 返り、保存済み Run の案内 (`RestorePrompt`) すら出ない。
        self.workspace = PathBuf::new();
        self.read_only = false;
        self.notice.clear();
        self.dropped_effects = 0;
        self.seen_order.clear();
        self.seen_set.clear();
        self.next_scan = None;
        self.next_launch_poll = None;
        self.dirty = true;
        true
    }

    /// **その場でプロセスツリーごと落とす** (アプリの終了時)。
    ///
    /// 札を立てて worker に任せる余裕が無いときに使う。戻りは落とした件数。
    /// 既存の [`crate::procx::kill_tree`] を使う — Team 専用の第 2 の
    /// プロセス管理は作らない。
    pub fn stop_all_validations_now(&mut self) -> usize {
        let mut killed = 0usize;
        for j in std::mem::take(&mut self.validation_jobs) {
            j.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            let pid = j.pid.load(std::sync::atomic::Ordering::Relaxed);
            if pid != 0 {
                crate::procx::kill_tree(pid);
                killed += 1;
            }
        }
        killed
    }

    /// 終わった検証を取り込む。**待たない** (`try_recv`)。
    ///
    /// **送り手が消えた場合を握り潰さない。** worker が panic すると結果は
    /// 永久に来ないので、受け口を捨てるだけでは `validation.running` が
    /// true のまま残り、そのタスクは二度と進まない。
    pub fn collect_validations(&mut self) {
        if self.validation_jobs.is_empty() {
            return;
        }
        let now = super::model::now_secs();
        let mut still = Vec::new();
        let mut done: Vec<(String, TaskId, Vec<ValidationRun>)> = Vec::new();
        for job in std::mem::take(&mut self.validation_jobs) {
            // **最後の砦。** 実行器が自分で打ち切れず、切断も起きない
            // (worker がブロックしたまま) 経路をここで決着させる。
            // 猶予は実行器の時限 + コマンド数ぶん + 余白。
            let limit = job
                .timeout_secs
                .saturating_mul(job.commands.len().max(1) as u64)
                .saturating_add(WATCHDOG_SLACK_SECS);
            if now.saturating_sub(job.started_at) > limit {
                let runs = job
                    .commands
                    .iter()
                    .map(|c| {
                        ValidationRun::new(c, 124, super::model::ValidationOutcome::TimedOut)
                    })
                    .collect();
                job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                done.push((job.execution.clone(), job.task, runs));
                continue;
            }
            // **別の Run のものは採らない。** 発行と同じ形で持ち主を見る。
            if self.owner().as_ref() != Some(&job.owner) {
                job.cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                self.dropped_effects = self.dropped_effects.saturating_add(1);
                continue;
            }
            match job.rx.try_recv() {
                Ok((execution, task, runs)) => done.push((execution, task, runs)),
                Err(std::sync::mpsc::TryRecvError::Empty) => still.push(job),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // 結果は来ない。**失敗として戻す** — 待ち続けない。
                    let runs = job
                        .commands
                        .iter()
                        .map(|c| {
                            ValidationRun::new(
                                c,
                                125,
                                super::model::ValidationOutcome::RunnerDisconnected,
                            )
                        })
                        .collect();
                    done.push((job.execution.clone(), job.task, runs));
                }
            }
        }
        self.validation_jobs = still;
        for (execution, task, runs) in done {
            if let Some(rt) = self.runtime.as_mut() {
                rt.note_validation_for(&execution, task, runs);
            }
            self.needs_save = true;
            self.dirty = true;
        }
    }

    /// 読み取り専用で開いているか。
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// 保存が要るなら保存する。**要らないときは 1 バイトも書かない。**
    pub fn save_if_needed(&mut self) {
        if !self.needs_save || self.read_only {
            return;
        }
        self.needs_save = false;
        let Some(rt) = self.runtime.as_ref() else {
            return;
        };
        let dir = self.state_dir();
        if let Err(e) = persistence::save(&dir, &rt.to_saved()) {
            self.notice = e.detail();
        }
    }

    /// スナップショットを作り直す (**変わったときだけ**)。
    pub fn refresh_snapshot(&mut self, now: u64) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        self.snapshot = self.runtime.as_ref().map(|rt| view_model::snapshot(rt, now));
    }

    /// 次のフレームでスナップショットを作り直す。
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }
}

/// FNV-1a 64bit。**版に依存しない**ハッシュ (`DefaultHasher` は rustc を
/// 上げると値が変わる — ここは 1 プロセス内でしか使わないが、揃えておく)。
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

thread_local! {
    /// UI スレッドが持つ Team の状態。
    ///
    /// **プロセス共通の `static` にしない。** テストは自分の
    /// [`TeamRuntime`] を作るので、ここへ触るのは GUI だけになる
    /// (共通にすると同時に走る他のテストの操作が混ざる)。
    static PANEL: RefCell<TeamPanel> = RefCell::new(TeamPanel::default());
}

/// UI スレッドの Team 状態へ触る。
pub fn with_panel<R>(f: impl FnOnce(&mut TeamPanel) -> R) -> R {
    PANEL.with(|p| f(&mut p.borrow_mut()))
}

/// **アプリが立ち上がったことを Team 状態へ知らせる** (`ZaivernApp::new`)。
///
/// この状態はアプリより長生きするので、前のアプリの Run が残っていることが
/// ありうる。[`TeamPanel::adopt_new_app_context`] が暗黙の引き継ぎを断つ。
/// 戻りは「残っていたものを手放したか」。
pub fn begin_app_context() -> bool {
    with_panel(|p| p.adopt_new_app_context())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(name: &str) -> PathBuf {
        crate::test_util::unique_temp_dir("zaivern-team-panel", name)
    }

    /// **実 `~/.zaivern` に 1 バイトも触らない**パネルを作る。
    ///
    /// 置き場の根を一時ディレクトリへ向ける。`ZAIVERN_HOME` を差し替える手も
    /// あるが、環境変数は並列に走る他のテストへ漏れるので採らない。
    fn panel_at(dir: &Path) -> TeamPanel {
        let mut p = TeamPanel::default();
        p.home = dir.join(".zaivern-test-home");
        p.attach_workspace(dir).expect("新しい画面は必ず attach できる");
        p
    }

    /// **検証を明示する SPEC。**
    ///
    /// ここで見たいのは画面の筋道であって、検証コマンドの自動決定ではない。
    /// 自動決定そのものは `validation_defaults` と `planner` の番人が見る。
    const SPEC: &str = "# 認証\n## 要件\n- A を作る (src/a.rs)\n## 検証\n- cargo test\n";

    #[test]
    fn 計画してから開始する() {
        let dir = ws("plan");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        assert!(p.has_run());
        // **計画しただけでは起動要求は出ない。**
        assert!(p.take_launches().is_empty());
        assert_eq!(p.goal_status().unwrap(), GoalStatus::Ready);
        p.act(TeamAction::Start);
        assert_eq!(p.goal_status().unwrap(), GoalStatus::Running);
        // 置き場もワークスペースの下なので、これ 1 行で全部片付く
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn フォームの入力はすべて計画に効く() {
        use super::super::model::TeamRole as R;
        let dir = ws("form-effect");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan_with(
            SPEC,
            "SPEC.md",
            RunOptions {
                agent_count: 2,
                max_attempts: 5,
                review_required: false,
                ..RunOptions::default()
            },
            vec![R::Architect, R::Implementer],
            "私が付けた名前",
        )
        .unwrap();
        let rt = p.runtime.as_ref().unwrap();
        // Goal 名が効く
        assert_eq!(rt.goal().title, "私が付けた名前");
        // 役割の選択が効く (設計レーンが立つ)
        assert!(rt.teams().iter().any(|t| t.id.as_str() == "architecture"));
        // レビューを外したので QA レーンは立たない
        assert!(!rt.teams().iter().any(|t| t.id.as_str() == "qa"));
        // 最大試行回数と最大エージェント数が効く
        assert_eq!(rt.run().max_attempts, 5);
        assert_eq!(rt.run().agent_count, 2);
        assert!(!rt.run().review_required);
        // 置き場もワークスペースの下なので、これ 1 行で全部片付く
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 起動要求が出るところまで進めた画面。
    fn started_panel(dir: &Path) -> TeamPanel {
        std::fs::create_dir_all(dir).unwrap();
        let mut p = panel_at(dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        p.act(TeamAction::Start);
        p.pump(super::super::runtime::Observation {
            now: 1,
            sessions: Vec::new(),
        });
        p
    }

    #[test]
    fn 起動要求はrunのworkspaceを運ぶ() {
        // **Runtime が決めた実行先を、実行側が取り直さないための材料。**
        // 要求そのものに workspace が載っていなければ、app は
        // 「いまの画面のフォルダ」を見るしかなくなる。
        let dir = ws("launch-ws");
        let mut p = started_panel(&dir);
        let launches = p.take_launches();
        assert!(!launches.is_empty(), "起動要求が出ていない");
        for (_, spec) in &launches {
            assert_eq!(
                spec.workspace_root, p.workspace,
                "起動要求が Run の workspace を運んでいない"
            );
            // 飾りのフィールドを残さない (役割と名前は指示文と端末名に出る)。
            assert!(!spec.name.trim().is_empty(), "名前が空");
            assert!(!spec.team_id.0.trim().is_empty(), "所属チームが空");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 実行中は別のworkspaceへ切り替えない() {
        // **画面から消えたのに裏で動き続ける**エージェントと検証を作らない。
        let a = ws("switch-a");
        let b = ws("switch-b");
        std::fs::create_dir_all(&b).unwrap();
        let mut p = started_panel(&a);
        // 起動要求が残っている = まだ面倒を見ている
        assert!(p.live_work().is_busy(), "実行中と見なされていない");
        let before = p.workspace.clone();
        let err = p.attach_workspace(&b).expect_err("切り替えを許してしまった");
        assert!(!err.trim().is_empty(), "断った理由が空");
        assert_eq!(p.workspace, before, "workspace が変わってしまった");
        assert!(p.has_run(), "Runtime を捨ててしまった");
        assert!(!p.take_launches().is_empty(), "抱えていた仕事まで消えた");
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn 走っている検証があるうちは切り替えない() {
        let a = ws("switch-val-a");
        let b = ws("switch-val-b");
        std::fs::create_dir_all(&b).unwrap();
        let mut p = started_panel(&a);
        // 仕事を全部片付けてから、検証だけを走らせる。
        let _ = p.take_launches();
        let (_tx, rx) = std::sync::mpsc::channel();
        p.watch_validation(ValidationJob {
            owner: p.owner().expect("Run がある"),
            task: 1,
            execution: "x".into(),
            commands: vec!["cargo test a".into()],
            started_at: super::super::model::now_secs(),
            timeout_secs: 600,
            cancel: super::super::launch::new_cancel_flag(),
            pid: super::super::launch::new_pid_slot(),
            rx,
        });
        assert_eq!(p.live_work().validations, 1);
        assert!(p.attach_workspace(&b).is_err(), "検証を孤児にした");
        assert_eq!(p.running_validations(), 1, "検証の管理を手放した");
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn 面倒を見るものが無ければ切り替えられる() {
        // **`runtime.is_some()` で永久に断らない。** 計画しただけの Run は
        // 誰も動かしていないので、切り替えてよい。
        let a = ws("switch-idle-a");
        let b = ws("switch-idle-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let mut p = panel_at(&a);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        assert!(!p.live_work().is_busy(), "計画しただけで実行中と見なした");
        p.attach_workspace(&b).expect("切り替えられるべき");
        assert_eq!(p.workspace, b);
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn 別のrunの検証結果は受け取らない() {
        // 外向きの 4 つの口と同じ形で、**戻ってくる側にも持ち主を見る**。
        // 実行 ID にも `run_id` は入っているが、そちらは文字列の書式に
        // 頼った守りなので、構造でも止める。
        let dir = ws("owner-validation");
        let mut p = started_panel(&dir);
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = super::super::launch::new_cancel_flag();
        p.watch_validation(ValidationJob {
            owner: p.owner().expect("Run がある"),
            task: 1,
            execution: "old".into(),
            commands: vec!["cargo test a".into()],
            started_at: super::super::model::now_secs(),
            timeout_secs: 600,
            cancel: cancel.clone(),
            pid: super::super::launch::new_pid_slot(),
            rx,
        });
        // Run を作り直す (検査だけ黙らせる — 本番は `plan` が断る)。
        p.allow_replace_run_for_test();
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        // 前の Run の worker が結果を返してくる。
        tx.send(("old".into(), 1, vec![ValidationRun::passed("cargo test a")]))
            .unwrap();
        p.collect_validations();
        assert_eq!(p.running_validations(), 0, "前の Run のジョブを抱えたまま");
        assert!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            "手放すのに止めていない (プロセスが残る)"
        );
        let t = p
            .runtime
            .as_ref()
            .and_then(|rt| rt.task(1))
            .expect("タスク")
            .clone();
        assert!(
            t.validation.runs.is_empty(),
            "別の Run の実測を採った: {:?}",
            t.validation.runs
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 実行中のrunを別の計画で潰さない() {
        // 置き換えると、走っている検証と起動済みのエージェントの面倒を
        // 見る相手が消える (結果は `run_id` 違いで捨てられ、プロセスだけが
        // 残る)。同じ workspace なので `attach_workspace` では守れない。
        let dir = ws("replace-busy");
        let mut p = started_panel(&dir);
        assert!(p.live_work().is_busy());
        let before = p.owner().expect("持ち主");
        let err = p
            .plan(SPEC, "SPEC.md", RunOptions::default())
            .expect_err("実行中なのに作り直せてしまった");
        assert!(!err.trim().is_empty(), "断った理由が空");
        assert_eq!(p.owner().as_ref(), Some(&before), "Run が入れ替わった");
        // 片付ければ作り直せる (永久に作れないわけではない)。
        let _ = p.take_launches();
        assert!(!p.live_work().is_busy());
        p.plan(SPEC, "SPEC.md", RunOptions::default())
            .expect("片付けたら作り直せるべき");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 別のrunのeffectは実行させない() {
        // 切り替えを断る仕組みだけに頼らない。**持ち主が違う Effect は
        // 渡さない**という構造の検査 (キューが空になる偶然に頼らない)。
        let a = ws("owner-a");
        let b = ws("owner-b");
        std::fs::create_dir_all(&b).unwrap();
        let mut p = started_panel(&a);
        assert!(!p.pending_launches.is_empty());
        // 別の Run へ差し替える (workspace ごと作り直す = 切り替えと同じ状況)。
        // 断りを外しても**持ち主の照合が効く**ことを見たいので、検査だけを
        // 黙らせて Run を作り直す (本番は `plan` が断る)。
        let stale = p.pending_launches.clone();
        p.workspace = b.clone();
        p.allow_replace_run_for_test();
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        p.pending_launches = stale;
        assert!(
            p.take_launches().is_empty(),
            "前の Run の起動要求を新しい Run で実行しようとした"
        );
        assert!(p.dropped_effects() > 0, "捨てたことを数えていない");
        assert!(!p.notice.trim().is_empty(), "黙って捨てている");
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn 指示と停止と検証も持ち主で選り分ける() {
        let a = ws("owner-all");
        let b = ws("owner-all-b");
        std::fs::create_dir_all(&b).unwrap();
        let mut p = started_panel(&a);
        // 4 つの口すべてに、前の Run のものを積む。
        let owner = p.owner().expect("持ち主");
        p.pending_instructions
            .push((owner.clone(), "instr:x".into(), (1, 7, "hi".into())));
        p.pending_stops.push((owner.clone(), "stop:7".into(), 7));
        p.pending_validations.push((
            owner,
            "validate:x".into(),
            super::super::runtime::ValidationSpec {
                task: 1,
                execution: "x".into(),
                commands: vec![super::super::validation_command::ValidationCommand::parse(
                    "cargo test a",
                )
                .unwrap()],
                approved: Vec::new(),
                cwd: a.clone(),
                timeout_secs: 600,
            },
        ));
        let stale = (
            p.pending_launches.clone(),
            p.pending_instructions.clone(),
            p.pending_stops.clone(),
            p.pending_validations.clone(),
        );
        p.workspace = b.clone();
        p.allow_replace_run_for_test();
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        p.pending_launches = stale.0;
        p.pending_instructions = stale.1;
        p.pending_stops = stale.2;
        p.pending_validations = stale.3;
        assert!(p.take_launches().is_empty(), "起動が漏れた");
        assert!(p.take_instructions().is_empty(), "指示が漏れた");
        assert!(p.take_stops().is_empty(), "停止が漏れた");
        assert!(p.take_validations().is_empty(), "検証が漏れた");
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn run破棄は走っている検証を先に止める() {
        let dir = ws("discard-cancel");
        let mut p = started_panel(&dir);
        let cancel = super::super::launch::new_cancel_flag();
        let (_tx, rx) = std::sync::mpsc::channel();
        p.watch_validation(ValidationJob {
            owner: p.owner().expect("Run がある"),
            task: 1,
            execution: "x".into(),
            commands: vec!["cargo test a".into()],
            started_at: super::super::model::now_secs(),
            timeout_secs: 600,
            cancel: cancel.clone(),
            pid: super::super::launch::new_pid_slot(),
            rx,
        });
        p.discard_run().expect("破棄できる");
        assert!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            "破棄したのに検証を止めていない (プロセスが残る)"
        );
        assert!(!p.has_run());
        // **Runtime を捨てても、まだ畳んでいないものは「面倒を見ている」。**
        // ここで 0 と答えると、直後の workspace 切り替えが通ってしまい、
        // 走っているプロセスの管理を手放す。
        assert_eq!(p.live_work().validations, 1, "破棄した瞬間に空と答えた");
        assert!(p.live_work().is_busy());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **実物のプロセスを 1 つ走らせて**、閉じたときに落ちることを見る。
    ///
    /// worker スレッドは**わざと置かない**。アプリを閉じる瞬間は、札を見る
    /// はずの worker ごと消える — そこで誰が木を落とすのか、が要点なので、
    /// worker が生きていると検査が空回りする (実際に、`kill_tree` を外して
    /// も worker が自分で落としてしまい緑のままだった)。
    #[cfg(unix)]
    #[test]
    fn 閉じると実際に検証プロセスが落ちる() {
        use std::os::unix::process::CommandExt;
        let dir = ws("shutdown-kill");
        let mut p = started_panel(&dir);
        let marker = dir.join("still-alive");
        // 落とすのは `stop_all_validations_now` の仕事なので、こちらでは
        // `wait` しない (clippy はそれを疑うので、意図を書いて許す)。
        #[allow(clippy::zombie_processes)]
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("sleep 1; : > {}", marker.display()))
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("子を起こせる");
        let pid = super::super::launch::new_pid_slot();
        pid.store(child.id(), std::sync::atomic::Ordering::Relaxed);
        let (_tx, rx) = std::sync::mpsc::channel();
        p.watch_validation(ValidationJob {
            owner: p.owner().expect("Run がある"),
            task: 1,
            execution: "x".into(),
            commands: vec!["cargo test a".into()],
            started_at: super::super::model::now_secs(),
            timeout_secs: 600,
            cancel: super::super::launch::new_cancel_flag(),
            pid,
            rx,
        });
        assert_eq!(p.stop_all_validations_now(), 1, "落とした件数が合わない");
        // 生き残っていれば 1 秒後にファイルを作る。
        std::thread::sleep(std::time::Duration::from_millis(1800));
        assert!(
            !marker.exists(),
            "閉じたのに検証プロセスが生き残った: {}",
            marker.display()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 閉じるときは保存して手放し検証も落とす() {
        // **状態はアプリより長生きする** (`thread_local!`)。持ったまま次の
        // アプリへ渡すと、もう居ないセッションへ結び付いた Runtime を
        // 新しい画面が見ることになる。
        let dir = ws("shutdown");
        let mut p = started_panel(&dir);
        let cancel = super::super::launch::new_cancel_flag();
        let (_tx, rx) = std::sync::mpsc::channel();
        p.watch_validation(ValidationJob {
            owner: p.owner().expect("Run がある"),
            task: 1,
            execution: "x".into(),
            commands: vec!["cargo test a".into()],
            started_at: super::super::model::now_secs(),
            timeout_secs: 600,
            cancel: cancel.clone(),
            pid: super::super::launch::new_pid_slot(),
            rx,
        });
        assert!(p.has_run());
        p.shutdown();
        assert!(!p.has_run(), "Runtime を手放していない");
        assert_eq!(p.running_validations(), 0, "検証を抱えたまま閉じた");
        assert!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            "検証を止めずに閉じた"
        );
        assert!(p.take_launches().is_empty(), "未実行の仕事が残っている");
        // 保存はされているので、次回は復元から入り直せる。
        assert!(
            persistence::has_run(&persistence::team_dir_in(&p.home, &dir)),
            "保存せずに手放した"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 実行側の返事が来るまで完了扱いにしない() {
        // **画面 (app) が Effect を実行し、成功を返して初めて済んだことになる。**
        // 返す前に落ちても、次の起動でもう一度出る。
        let dir = ws("ack");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        p.act(TeamAction::Start);
        p.pump(super::super::runtime::Observation {
            now: 1,
            sessions: Vec::new(),
        });
        let launches = p.take_launches();
        assert!(!launches.is_empty(), "起動要求が出ていない");
        // 冪等キーが必ず添えられている (返せないと ACK もできない)
        for (key, _) in &launches {
            assert!(key.starts_with("start:"), "冪等キーが無い: {key}");
        }
        // 失敗を返せば、次の tick でもう一度出る
        for (key, _) in &launches {
            p.ack_failed(key);
        }
        p.pump(super::super::runtime::Observation {
            now: 2,
            sessions: Vec::new(),
        });
        assert_eq!(
            p.take_launches().len(),
            launches.len(),
            "失敗を返したのに再発行されない"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 検証 1 件を「走っている」状態で預ける (受け口だけ返す)。
    fn queue_job(
        p: &mut TeamPanel,
        task: TaskId,
        execution: &str,
    ) -> std::sync::mpsc::Sender<(String, TaskId, Vec<ValidationRun>)> {
        let (tx, rx) = std::sync::mpsc::channel();
        p.watch_validation(ValidationJob {
            owner: p.owner().expect("Run がある"),
            task,
            execution: execution.to_string(),
            commands: vec!["cargo test a".into()],
            started_at: super::super::model::now_secs(),
            timeout_secs: 600,
            cancel: super::super::launch::new_cancel_flag(),
            pid: super::super::launch::new_pid_slot(),
            rx,
        });
        tx
    }

    #[test]
    fn 実行器との接続が切れたら失敗として戻す() {
        // **握り潰さない。** 受け口を捨てるだけだと `validation.running` が
        // true のまま残り、そのタスクは二度と進まない。
        let dir = ws("disconnect");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        let exec = p
            .runtime
            .as_ref()
            .expect("runtime")
            .current_execution(1);
        let tx = queue_job(&mut p, 1, &exec);
        assert_eq!(p.running_validations(), 1);
        drop(tx); // worker が panic した状況
        p.collect_validations();
        assert_eq!(p.running_validations(), 0, "受け口を抱えたまま");
        let t = p
            .runtime
            .as_ref()
            .and_then(|rt| rt.task(1))
            .expect("タスク")
            .clone();
        assert!(!t.validation.running, "実行中のまま残った");
        assert!(
            t.validation
                .runs
                .iter()
                .any(|r| r.outcome() == super::super::model::ValidationOutcome::RunnerDisconnected),
            "接続断を記録していない: {:?}",
            t.validation.runs
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 結果も切断も来ない検証は見張りが決着させる() {
        // 実行器が自分で打ち切れず、送り手も生きたまま黙っている経路
        // (worker がブロックした)。**ここを開けたままにすると、
        // `validation.running` が永久に true のまま残る。**
        let dir = ws("watchdog");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        let exec = p.runtime.as_ref().expect("runtime").current_execution(1);
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = super::super::launch::new_cancel_flag();
        p.watch_validation(ValidationJob {
            owner: p.owner().expect("Run がある"),
            task: 1,
            execution: exec,
            commands: vec!["cargo test a".into()],
            // 時限 + 余白より前に始まっている = もう見切ってよい。
            started_at: super::super::model::now_secs()
                .saturating_sub(60 + WATCHDOG_SLACK_SECS + 10),
            timeout_secs: 60,
            cancel: cancel.clone(),
            pid: super::super::launch::new_pid_slot(),
            rx,
        });
        p.collect_validations();
        assert_eq!(p.running_validations(), 0, "見切っていない");
        assert!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            "見切ったのに停止の札を立てていない (プロセスが残る)"
        );
        let t = p
            .runtime
            .as_ref()
            .and_then(|rt| rt.task(1))
            .expect("タスク")
            .clone();
        assert!(!t.validation.running);
        assert!(
            t.validation
                .runs
                .iter()
                .any(|r| r.outcome() == super::super::model::ValidationOutcome::TimedOut),
            "時間切れとして記録していない: {:?}",
            t.validation.runs
        );
        drop(tx);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 停止の要求は走っている検証にだけ届く() {
        let dir = ws("cancel");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        let _tx = queue_job(&mut p, 1, "run:1:0:1");
        assert!(!p.cancel_validation("run:1:0:99"), "世代違いまで止めた");
        assert!(p.cancel_validation("run:1:0:1"), "止められなかった");
        assert_eq!(p.running_validations(), 1, "札を立てただけで捨てない");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 読み取り専用では操作できない() {
        let dir = ws("ro");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        p.read_only = true;
        p.act(TeamAction::Start);
        assert_eq!(p.goal_status().unwrap(), GoalStatus::Ready);
        assert!(p.notice.contains("読み取り専用"));
        // 置き場もワークスペースの下なので、これ 1 行で全部片付く
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 保存して復元すると未完了runとして出る() {
        let dir = ws("restore");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let mut p = panel_at(&dir);
            p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
            p.act(TeamAction::Start);
            p.save_if_needed();
        }
        let mut q = panel_at(&dir);
        assert_eq!(q.restore, RestorePrompt::Found, "未完了 Run を検出していない");
        q.restore_run(false).unwrap();
        assert!(q.has_run());
        assert_eq!(q.restore, RestorePrompt::None);
        assert!(q.goal_status().is_some());
        // 置き場もワークスペースの下なので、これ 1 行で全部片付く
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 不正なspecは計画に失敗する() {
        let dir = ws("bad");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        assert!(p.plan("   ", "SPEC.md", RunOptions::default()).is_err());
        assert!(!p.has_run(), "失敗したのに Run ができている");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn スナップショットは変わったときだけ作り直す() {
        let dir = ws("snap");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        p.refresh_snapshot(100);
        let a = p.snapshot().cloned().expect("スナップショット");
        // 何も変えずに呼んでも中身は同じ (作り直しても等価)
        p.refresh_snapshot(101);
        assert_eq!(p.snapshot().unwrap(), &a);
        p.act(TeamAction::Start);
        p.refresh_snapshot(102);
        assert_ne!(p.snapshot().unwrap().goal.status, a.goal.status);
        // 置き場もワークスペースの下なので、これ 1 行で全部片付く
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn フォームの初期値は仕様どおり() {
        let f = NewRunForm::default();
        assert_eq!(f.agents, 4);
        assert_eq!(f.max_attempts, 3);
        assert!(f.review_required);
        assert_eq!(f.approval_mode, "ask");
        // 既定のプリセットは実装 + レビュー
        assert_eq!(f.roles, vec![TeamRole::Implementer, TeamRole::Reviewer]);
    }

    #[test]
    fn 役割プリセットは選べる全役割の部分集合() {
        let f = NewRunForm::default();
        for r in &f.roles {
            assert!(TeamRole::ALL.contains(r), "{} は選択肢に無い", r.key());
        }
    }

    #[test]
    fn タブは同時に一つ() {
        let mut p = TeamPanel::default();
        assert_eq!(p.tab, BoardTab::Organization);
        p.tab = BoardTab::Tasks;
        assert_eq!(p.tab, BoardTab::Tasks);
        // 4 つのタブ ID は重複しない
        let mut seen = std::collections::BTreeSet::new();
        for t in BoardTab::ALL {
            assert!(seen.insert(t.key()));
        }
    }

    #[test]
    fn 同じ行を二度読まない() {
        let mut p = TeamPanel::default();
        let tail = vec!["a".to_string(), "b".to_string()];
        assert_eq!(p.new_lines(1, &tail), "a\nb\n");
        // 2 回目は 1 行も返さない (毎フレーム同じ報告を読み直さない)
        assert_eq!(p.new_lines(1, &tail), "");
        let more = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(p.new_lines(1, &more), "c\n");
        // 別のセッションは独立
        assert_eq!(p.new_lines(2, &tail), "a\nb\n");
    }

    #[test]
    fn 記憶は上限を超えない() {
        let mut p = TeamPanel::default();
        for i in 0..(SEEN_LINES_CAP + 100) {
            p.new_lines(1, &[format!("line {i}")]);
        }
        assert!(p.seen_order[&1].len() <= SEEN_LINES_CAP);
        assert!(p.seen_set[&1].len() <= SEEN_LINES_CAP);
    }

    #[test]
    fn 消えたセッションの記憶は捨てる() {
        let mut p = TeamPanel::default();
        p.new_lines(1, &["x".to_string()]);
        assert!(p.seen_set.contains_key(&1));
        p.forget_session(1);
        assert!(!p.seen_set.contains_key(&1));
    }

    #[test]
    fn 起動要求の確認も間隔を空ける() {
        let mut p = TeamPanel::default();
        let t0 = Instant::now();
        assert!(p.launch_poll_due(t0), "初回は見に行く");
        assert!(!p.launch_poll_due(t0), "毎フレーム stat を撃たない");
        assert!(p.launch_poll_due(t0 + LAUNCH_POLL_INTERVAL + Duration::from_millis(1)));
    }

    #[test]
    fn 走査は間隔を空ける() {
        let mut p = TeamPanel::default();
        let t0 = Instant::now();
        assert!(p.scan_due(t0), "初回は走る");
        assert!(!p.scan_due(t0), "同じ瞬間に二度は走らない");
        assert!(p.scan_due(t0 + SCAN_INTERVAL + Duration::from_millis(1)));
    }

    /// **制御面を投函から再開まで 1 本で通す** (どの OS でも同じ経路)。
    ///
    /// GUI そのものは描けないので、GUI の直下 — 起動要求 / 計画 / 開始 /
    /// 起動 Effect / 担当割当 / 検証 Effect / 停止と再開 / 保存と復元 —
    /// を繋げて確かめる。**`cfg` で OS を分けない**ので、Windows の CI でも
    /// そのまま走る: パスの区切り (`\`) と、`~/.zaivern` 配下の置き場と、
    /// JSON へ往復するパスの綴りが、この 1 本で全部通ることになる。
    ///
    /// 個々の段の細かい振る舞いはそれぞれの番人が見ているので、ここは
    /// **鎖が繋がっていること**だけを見る (どこかが切れたら落ちる)。
    #[test]
    fn 制御面は投函から再開までどのosでも通る() {
        use super::super::launch;

        let dir = ws("os-chain");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("a.rs"), "fn a() {}\n").unwrap();
        // **検証は「読むだけ」のものにする。** リポジトリのコードを走らせる
        // ものだと承認待ちで止まり、検証 Effect まで届かない。
        let spec_body = "# 認証\n## 要件\n- A を作る (src/a.rs)\n## 検証\n- rustfmt --check src/a.rs\n";
        let spec_path = dir.join("SPEC.md");
        std::fs::write(&spec_path, spec_body).unwrap();
        // **置き場は workspace の外**に置く。中に置くと自分の書いた記録が
        // `git status` に出て、実測が「担当外の変更」として拾ってしまう。
        let home = ws("os-chain-home");
        let now = super::super::model::now_secs();

        // 完了報告は**実測**を通る (`git` が要る)。綺麗なリポジトリから
        // 始めるので、報告そのものの受理まで届く。
        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            eprintln!("[skip] 制御面は投函から再開まで — git を使えません");
            std::fs::remove_dir_all(&dir).ok();
            return;
        }
        git(&["config", "user.email", "t@example.invalid"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);
        assert!(
            git(&["add", "-A"]) && git(&["commit", "-q", "-m", "init"]),
            "実験場を commit できない"
        );

        // ── 1) 投函 → 拾える。境界の外の SPEC は組み立てで断る ───────────
        let req = launch::build(&dir, &spec_path, 2, false).expect("投函を組み立てられる");
        assert_eq!(req.agent_count, 2);
        assert!(
            launch::request_matches_workspace(&req, &dir),
            "この workspace 宛ての要求を受け取れない"
        );
        launch::post_in(&home, &req).expect("投函できる");
        let got = launch::take_in(&home, &dir, now).expect("投函を拾えない");
        assert_eq!(got.spec_text, spec_body, "SPEC の中身が変わった");
        assert!(
            launch::take_in(&home, &dir, now).is_none(),
            "同じ投函を二度拾った"
        );
        let outside_dir = ws("os-chain-outside");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let outside = outside_dir.join("SPEC.md");
        std::fs::write(&outside, spec_body).unwrap();
        assert!(
            launch::build(&dir, &outside, 2, false).is_err(),
            "workspace の外の SPEC を受理した"
        );

        // ── 2) 計画 (agents=2) ────────────────────────────────────────────
        let mut p = TeamPanel::default();
        p.home = home.clone();
        p.attach_workspace(&dir).expect("attach できる");
        p.plan(
            &got.spec_text,
            "SPEC.md",
            RunOptions {
                agent_count: 2,
                ..RunOptions::default()
            },
        )
        .expect("計画できる");
        assert_eq!(p.runtime.as_ref().expect("Runtime").run().agent_count, 2);

        // ── 3) 開始 → 起動要求。パスはこの OS の綴りのまま運ばれる ───────
        p.act(TeamAction::Start);
        assert_eq!(p.goal_status(), Some(GoalStatus::Running));
        // 開始しただけでは何も起きない。**調停ループが 1 度回って**初めて
        // 「誰を起こすか」が決まる (押した瞬間に副作用を出さない)。
        p.pump(super::super::runtime::Observation {
            now,
            sessions: Vec::new(),
        });
        let launches = p.take_launches();
        assert!(!launches.is_empty(), "起動要求が 1 つも出ない");
        let mut agents = Vec::new();
        for (key, spec) in &launches {
            assert_eq!(spec.workspace_root, dir, "起動先の workspace がずれた");
            agents.push(spec.agent_id.clone());
            p.ack_done(key);
        }

        // ── 4) セッションを結び付ける → 担当が付く ────────────────────────
        let sessions: Vec<SessionId> = (0..agents.len() as SessionId).map(|i| 100 + i).collect();
        for (a, s) in agents.iter().zip(&sessions) {
            p.bind_session(a, *s, None);
        }
        let rows = |target: Option<SessionId>, text: &str| -> Vec<SessionInput> {
            sessions
                .iter()
                .map(|s| SessionInput {
                    id: *s,
                    title: format!("agent{s}"),
                    provider: "claude".into(),
                    state: crate::coordinator::SessionState::Idle,
                    tail: if Some(*s) == target {
                        text.lines().map(|l| l.to_string()).collect()
                    } else {
                        Vec::new()
                    },
                })
                .collect()
        };
        p.pump_sessions(rows(None, ""), now + 1);
        let working: Vec<(TaskId, SessionId, String)> = p
            .runtime
            .as_ref()
            .expect("Runtime")
            .tasks()
            .iter()
            .filter(|t| t.state.is_working())
            .filter_map(|t| {
                Some((
                    t.id,
                    t.assigned_session?,
                    t.assigned_agent.as_ref()?.0.clone(),
                ))
            })
            .collect();
        assert!(!working.is_empty(), "担当が 1 つも付かない");

        // ── 5) 完了報告 → 検証 Effect (cwd はこの workspace) ──────────────
        let (tid, sid, agent) = working[0].clone();
        let t = p.runtime.as_ref().unwrap().task(tid).expect("タスク").clone();
        let files: Vec<String> = t.files.iter().map(|x| format!("\"{x}\"")).collect();
        let vs: Vec<String> = t
            .validation_commands
            .iter()
            .map(|c| format!("{{\"command\":\"{c}\",\"exit_code\":0}}"))
            .collect();
        let report = format!(
            "{open}\n{{\"task_id\":{tid},\"agent_id\":\"{agent}\",\"status\":\"completed\",\
             \"summary\":\"実装しました\",\"changed_files\":[{f}],\"validation\":[{v}],\"blockers\":[]}}\n{close}",
            open = super::super::result_parser::RESULT_OPEN,
            close = super::super::result_parser::RESULT_CLOSE,
            f = files.join(","),
            v = vs.join(","),
        );
        p.pump_sessions(rows(Some(sid), &report), now + 2);
        let validations = p.take_validations();
        assert!(!validations.is_empty(), "検証の要求が出ない");
        assert_eq!(validations[0].1.cwd, dir, "検証の cwd がずれた");
        assert!(
            !validations[0].1.commands.is_empty(),
            "空の検証を頼んでいる"
        );

        // ── 6) 停止と再開が状態機械の上で成立する ─────────────────────────
        p.act(TeamAction::Pause);
        assert_eq!(p.goal_status(), Some(GoalStatus::Paused), "止まらない");
        p.act(TeamAction::Resume);
        assert_eq!(p.goal_status(), Some(GoalStatus::Running), "戻らない");
        p.act(TeamAction::Stop);
        assert!(
            p.runtime.as_ref().expect("Runtime").run().stopped,
            "停止が効かない"
        );

        // ── 7) 保存して復元できる (パスの綴りごと往復する) ────────────────
        p.save_if_needed();
        let saved_ws = p.workspace().to_path_buf();
        drop(p);
        let mut q = TeamPanel::default();
        q.home = home.clone();
        q.attach_workspace(&dir).expect("attach できる");
        assert_eq!(q.restore, RestorePrompt::Found, "保存済み Run を見つけられない");
        q.restore_run(false).expect("復元できる");
        assert!(q.has_run(), "復元しても Run が無い");
        assert_eq!(
            q.owner().expect("持ち主").workspace,
            saved_ws,
            "往復で workspace の綴りが変わった"
        );

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside_dir).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// **Team Run の作成で、利用者のグローバル設定を変えない。**
    ///
    /// フォームの既定は `ask` / `0`。`0` は**このコードベースでは
    /// 「上限なし」**なので、既存設定を読まずに流すと、`agent` / `25` で
    /// 使っている人がフォームを開いて計画しただけで承認モードが下がり、
    /// 課金の上限が永続的に外れる。
    #[test]
    fn team_runの計画は既存のguardrailsを初期値にして書き換えない() {
        use super::super::model::RunGuardrails;

        // ── ケース A: フォームは既存設定を初期値として持つ ───────────────
        let dir = ws("guardrails");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        // 既定は「読んでいない」値のまま。
        assert_eq!(p.form.approval_mode, "ask");
        assert_eq!(p.form.cost_limit, 0.0);
        p.seed_guardrails("agent", 25.0);
        assert_eq!(p.form.approval_mode, "agent", "既存の承認モードを読んでいない");
        assert_eq!(p.form.cost_limit, 25.0, "既存のコスト上限を読んでいない");
        // **開いている間は上書きしない** (人が選び直した値を戻さない)。
        p.form.open = true;
        p.form.approval_mode = "ask".into();
        p.seed_guardrails("agent", 25.0);
        assert_eq!(p.form.approval_mode, "ask", "入力中の値を上書きした");
        p.form.open = false;

        // ── ケース B: Run 側で ask / 0 を選んでも、既存の値は緩まない ────
        let run = RunGuardrails {
            approval_mode: "ask".into(),
            cost_limit: 0.0,
        };
        assert_eq!(
            run.effective_approval("agent"),
            "ask",
            "Run 側の厳しい選択が効いていない"
        );
        assert_eq!(
            run.effective_cost_limit(25.0),
            25.0,
            "Run 側の 0 で既存の上限が外れた (0 は「上限なし」)"
        );

        // **緩める方向には効かない。**
        let loose = RunGuardrails {
            approval_mode: "auto".into(),
            cost_limit: 100.0,
        };
        assert_eq!(
            loose.effective_approval("ask"),
            "ask",
            "Run 側の指定で承認モードが緩んだ"
        );
        assert_eq!(
            loose.effective_cost_limit(25.0),
            25.0,
            "Run 側の大きい上限で既存の上限が緩んだ"
        );
        // 既存に上限が無ければ、Run 側の上限がそのまま効く (締める方向)。
        assert_eq!(loose.effective_cost_limit(0.0), 100.0);
        // 空 = 何も足さない。
        let none = RunGuardrails::default();
        assert_eq!(none.effective_approval("agent"), "agent");
        assert_eq!(none.effective_cost_limit(25.0), 25.0);
        // 読めない綴りは**いちばん厳しい側**に倒す。
        let junk = RunGuardrails {
            approval_mode: "yolo".into(),
            cost_limit: 0.0,
        };
        assert!(
            crate::agents::Approval::from_mode(&junk.effective_approval("auto"))
                == crate::agents::Approval::Ask,
            "読めない綴りを自動側へ倒している"
        );
        assert_eq!(
            super::super::model::approval_looseness("yolo"),
            super::super::model::approval_looseness("ask"),
            "読めない綴りをいちばん厳しい側に置いていない"
        );

        // ── ケース C: Run 固有設定は保存・復元をまたいで同じ ──────────────
        p.plan_with(
            SPEC,
            "SPEC.md",
            RunOptions {
                guardrails: RunGuardrails {
                    approval_mode: "ask".into(),
                    cost_limit: 5.0,
                },
                ..RunOptions::default()
            },
            Vec::new(),
            "",
        )
        .expect("計画できる");
        assert_eq!(
            p.run_guardrails(),
            Some(RunGuardrails {
                approval_mode: "ask".into(),
                cost_limit: 5.0,
            }),
            "Run へ運ばれていない"
        );
        p.act(TeamAction::Start);
        p.save_if_needed();
        let mut q = TeamPanel::default();
        q.home = p.home.clone();
        q.attach_workspace(&dir).expect("attach できる");
        q.restore_run(false).expect("復元できる");
        assert_eq!(
            q.run_guardrails(),
            Some(RunGuardrails {
                approval_mode: "ask".into(),
                cost_limit: 5.0,
            }),
            "復元で Run 固有設定が失われた"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **この欄が無い旧 Run も読める** (`serde(default)`)。
    #[test]
    fn guardrailsを持たない旧runも読める() {
        let doc: super::super::persistence::RunDoc =
            serde_json::from_str(r#"{"version":4,"run_id":"r","workspace":"/w","spec_source":"s","agent_count":2,"max_attempts":3,"review_required":true,"paused":false,"stopped":false,"started_at":0,"updated_at":0}"#)
                .expect("旧 Run が読めない");
        assert_eq!(doc.guardrails, super::super::model::RunGuardrails::default());
    }

    /// **積めたことを「届いた」にしない。**
    ///
    /// 送信経路は「積めた」と「届いた」を別の時刻に決める。積んだ時点で
    /// 冪等キーを完了にすると、そのあと相手が消えても・入力欄が空かない
    /// まま上限に達しても、Runtime は「指示は届いた」と信じたままタスクを
    /// 抱え続ける (二度と出し直されない = 指示が消える)。
    #[test]
    fn 届かなかった指示は完了にせず配り直せる形へ戻す() {
        let dir = ws("delivery");
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = panel_at(&dir);
        p.plan(SPEC, "SPEC.md", RunOptions::default()).unwrap();
        p.act(TeamAction::Start);
        p.pump(super::super::runtime::Observation {
            now: 1,
            sessions: Vec::new(),
        });
        let launches = p.take_launches();
        assert!(!launches.is_empty(), "前提: 起動要求が出ている");
        let mut sessions: Vec<SessionId> = Vec::new();
        for (i, (key, spec)) in launches.iter().enumerate() {
            p.ack_done(key);
            let sid = 700 + i as SessionId;
            p.bind_session(&spec.agent_id, sid, Some(format!("/logs/{sid}.log")));
            sessions.push(sid);
        }
        let rows = || -> Vec<SessionInput> {
            sessions
                .iter()
                .map(|s| SessionInput {
                    id: *s,
                    title: format!("agent{s}"),
                    provider: "claude".into(),
                    state: crate::coordinator::SessionState::Idle,
                    tail: Vec::new(),
                })
                .collect()
        };
        p.pump_sessions(rows(), 2);
        let sent = p.take_instructions();
        assert!(!sent.is_empty(), "前提: 指示が出ている");
        let (key, task, _, _) = sent[0].clone();
        let tag = p.delivery_tag(&key).expect("Run があるので目印は作れる");

        // ── 届かなかった ────────────────────────────────────────────────
        let hit = p.note_delivery(&tag, false);
        assert_eq!(hit, Some(task), "どのタスクが届かなかったか返していない");
        let rt = p.runtime.as_ref().expect("Runtime");
        assert!(
            !rt.effect_completed(&key),
            "届いていないのに完了として記録した (二度と出し直されない)"
        );
        let t = rt.task(task).expect("タスク");
        assert!(
            t.assigned_session.is_none() && t.assigned_agent.is_none(),
            "届かなかったのに担当を握ったまま: {t:?}"
        );
        assert!(
            t.context.iter().any(|c| c.contains("届きません")),
            "理由が残っていない: {:?}",
            t.context
        );

        // ── 届いた ──────────────────────────────────────────────────────
        p.pump_sessions(rows(), 3);
        let again = p.take_instructions();
        assert!(!again.is_empty(), "配り直しの指示が出ない");
        let (key2, _, _, _) = again[0].clone();
        let tag2 = p.delivery_tag(&key2).expect("目印");
        assert_eq!(p.note_delivery(&tag2, true), None, "届いたのに理由を返した");
        assert!(
            p.runtime
                .as_ref()
                .expect("Runtime")
                .effect_completed(&key2),
            "届いたのに完了として記録していない (もう一度送ってしまう)"
        );
        // **Team のものではない目印は何も起こさない。**
        assert_eq!(p.note_delivery("submit:someone-else", false), None);
        // **別の Run の配達は、いまの Run へ効かない。**
        let other = format!("run-other|{key2}");
        assert_eq!(p.note_delivery(&other, false), None, "別の Run の配達を採った");
        assert!(
            p.runtime.as_ref().expect("Runtime").effect_completed(&key2),
            "別の Run の配達でいまの Run の記録を壊した"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn team_state_does_not_leak_across_workspaces_or_app_contexts() {
        // 状態は `thread_local!` に居て **`ZaivernApp` より長生きする**。
        // 「UI スレッドあたり 1 インスタンス」という前提が破れた日
        // (前のアプリが `on_exit` を通らずに消えた / 同じスレッドに 2 つ目が
        // 立った) に、新しいアプリが前の Run を暗黙に引き継がないことを見る。
        let a = ws("leak-a");
        let b = ws("leak-b");
        std::fs::create_dir_all(&b).unwrap();

        // ── アプリ A: workspace A で Run を作り、セッションまで結び付ける ──
        let mut p = started_panel(&a);
        let owner_a = p.owner().expect("A の持ち主");
        let launches = p.take_launches();
        assert!(!launches.is_empty(), "前提: 起動要求が出ている");
        // **アプリ A のセッションへ結び付ける。** ここを省くと「復元しても
        // 前のアプリのセッションへ結び付かない」の検査が空回りする
        // (結び付いていないものは、外れていて当たり前)。
        let mut sessions: Vec<SessionId> = Vec::new();
        for (i, (key, spec)) in launches.iter().enumerate() {
            p.ack_done(key);
            let sid = 900 + i as SessionId;
            p.bind_session(&spec.agent_id, sid, None);
            sessions.push(sid);
        }
        // **観測にも同じセッションを出す。** 出さないと「消えた」と見なされ、
        // 結び付きが解かれてしまう (この検査が空回りする)。
        p.pump_sessions(
            sessions
                .iter()
                .map(|s| SessionInput {
                    id: *s,
                    title: format!("agent{s}"),
                    provider: "claude".into(),
                    state: crate::coordinator::SessionState::Idle,
                    tail: Vec::new(),
                })
                .collect(),
            2,
        );
        assert!(
            p.runtime
                .as_ref()
                .expect("Runtime")
                .agents()
                .iter()
                .any(|ag| ag.session_id.is_some()),
            "前提: アプリ A のセッションへ結び付いている"
        );
        let cancel = super::super::launch::new_cancel_flag();
        let (_tx, rx) = std::sync::mpsc::channel();
        p.watch_validation(ValidationJob {
            owner: owner_a.clone(),
            task: 1,
            execution: "x".into(),
            commands: vec!["cargo test a".into()],
            started_at: super::super::model::now_secs(),
            timeout_secs: 600,
            cancel: cancel.clone(),
            pid: super::super::launch::new_pid_slot(),
            rx,
        });
        assert!(p.live_work().is_busy(), "前提: 面倒を見ているものがある");

        // ── workspace 軸: 走っている間は別の workspace へ渡さない ────────
        let err = p
            .attach_workspace(&b)
            .expect_err("実行中なのに別の workspace へ渡した");
        assert!(!err.trim().is_empty(), "断った理由が空");
        assert_eq!(p.owner().as_ref(), Some(&owner_a), "Run が入れ替わった");

        // ── アプリ文脈の軸: 新しいアプリは前の Run を引き継がない ────────
        assert!(p.adopt_new_app_context(), "残っていたものを手放していない");
        assert!(!p.has_run(), "新しいアプリが前の Run を握ったままになった");
        assert_eq!(p.owner(), None, "持ち主が残っている");
        assert_eq!(p.running_validations(), 0, "検証を抱えたまま引き継いだ");
        assert!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            "面倒を見る相手が居なくなったのに検証を止めていない"
        );
        // **保存はされている。** 続きは復元の案内から入り直せる
        // (「引き継がない」は「捨てる」ではない)。
        assert!(
            persistence::has_run(&persistence::team_dir_in(&p.home, &a)),
            "保存せずに手放した"
        );

        // ── 引き継ぎ後: 同じ workspace を開き直しても、入り直しになる ────
        // `workspace` を空へ戻していないと「同じだから何もしない」で
        // 早々に返り、保存済み Run の案内すら出ない。
        p.attach_workspace(&a).expect("新しいアプリは attach できる");
        assert!(!p.has_run(), "attach しただけで前の Run が復活した");
        assert_eq!(
            p.restore,
            RestorePrompt::Found,
            "保存済み Run の案内が出ない (入り直せない)"
        );

        // ── 復元しても、前のアプリのセッションへは結び付かない ───────────
        // ここが「引き継がない」ことの実質。`run_id` は同じ Run なので
        // 引き継ぐが (それが復元の意味)、**プロセスへの結び付きは全部外れる**
        // ので、新しいアプリが前のアプリの端末を操作することはない。
        p.restore_run(false).expect("復元できるべき");
        let rt = p.runtime.as_ref().expect("復元した Runtime");
        assert!(
            rt.tasks()
                .iter()
                .all(|t| t.assigned_session.is_none() && t.coordinator_task.is_none()),
            "前のアプリのセッションへ結び付いたまま復元した: {:?}",
            rt.tasks()
                .iter()
                .map(|t| (t.id, t.assigned_session))
                .collect::<Vec<_>>()
        );
        assert!(
            rt.agents().iter().all(|a| a.session_id.is_none()),
            "前のアプリのセッションを持つエージェントが残った"
        );
        // 前のアプリが残した仕事も 1 つも持っていない。
        assert!(
            p.take_launches().is_empty(),
            "前のアプリの起動要求が残っている"
        );
        assert!(p.take_validations().is_empty(), "前のアプリの検証が残っている");

        // ── 普通の起動 (何も残っていない) では 1 命令も走らない ──────────
        let mut fresh = TeamPanel::default();
        assert!(
            !fresh.adopt_new_app_context(),
            "何も無いのに後始末を走らせた"
        );

        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn スレッドローカルの状態は独立して触れる() {
        with_panel(|p| p.notice = "hello".into());
        with_panel(|p| assert_eq!(p.notice, "hello"));
        with_panel(|p| p.notice.clear());
    }
}
