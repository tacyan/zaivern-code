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
use super::runtime::{Observation, RunOptions, TeamAction, TeamEffect, TeamRuntime};
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
    /// 結果の受け口 (実行 ID, タスク, 実測)。
    pub rx: std::sync::mpsc::Receiver<(String, TaskId, Vec<ValidationRun>)>,
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
    pending_launches: Vec<(String, super::runtime::AgentLaunchSpec)>,
    /// 実行を頼んだ検証。
    pending_validations: Vec<(String, super::runtime::ValidationSpec)>,
    /// 停止を頼んだセッション。
    pending_stops: Vec<(String, SessionId)>,
    /// 送るべき指示 (冪等キー, セッション ID, 本文)。
    pending_instructions: Vec<(String, SessionId, String)>,
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
    fn state_dir(&self) -> PathBuf {
        persistence::team_dir_in(&self.home, &self.workspace)
    }

    /// ワークスペースを設定し、保存済みの Run があれば知らせる。
    pub fn attach_workspace(&mut self, ws: &Path) {
        if self.workspace == ws {
            return;
        }
        self.workspace = ws.to_path_buf();
        self.runtime = None;
        self.snapshot = None;
        self.restore = if persistence::has_run(&self.state_dir()) {
            RestorePrompt::Found
        } else {
            RestorePrompt::None
        };
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
        for e in effects {
            let key = e.key();
            match e {
                TeamEffect::StartAgent(s) => self.pending_launches.push((key, s)),
                TeamEffect::SendInstruction { session, text, .. } => {
                    self.pending_instructions.push((key, session, text))
                }
                TeamEffect::StopAgent(s) => self.pending_stops.push((key, s)),
                TeamEffect::RunValidation(v) => self.pending_validations.push((key, v)),
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

    /// Effect の実行に失敗したと Runtime へ返す (次の tick で再発行される)。
    pub fn ack_failed(&mut self, key: &str) {
        if let Some(rt) = self.runtime.as_mut() {
            rt.note_effect_failed(key);
        }
        self.needs_save = true;
        self.dirty = true;
    }

    /// 起動してほしいエージェント (冪等キー付き。取り出したら消える)。
    pub fn take_launches(&mut self) -> Vec<(String, super::runtime::AgentLaunchSpec)> {
        std::mem::take(&mut self.pending_launches)
    }
    /// 送ってほしい指示 (冪等キー付き。取り出したら消える)。
    pub fn take_instructions(&mut self) -> Vec<(String, SessionId, String)> {
        std::mem::take(&mut self.pending_instructions)
    }
    /// 止めてほしいセッション (冪等キー付き。取り出したら消える)。
    pub fn take_stops(&mut self) -> Vec<(String, SessionId)> {
        std::mem::take(&mut self.pending_stops)
    }
    /// 走らせてほしい検証 (冪等キー付き。取り出したら消える)。
    pub fn take_validations(&mut self) -> Vec<(String, super::runtime::ValidationSpec)> {
        std::mem::take(&mut self.pending_validations)
    }

    /// 起動したセッションを結び付ける。
    pub fn bind_session(&mut self, agent: &AgentId, session: SessionId) {
        if let Some(rt) = self.runtime.as_mut() {
            rt.bind_session(agent, session);
        }
        self.dirty = true;
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
    pub fn cancel_all_validations(&mut self) -> usize {
        for j in &self.validation_jobs {
            j.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.running_validations()
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
        p.attach_workspace(dir);
        p
    }

    const SPEC: &str = "# 認証\n## 要件\n- A を作る (src/a.rs)\n";

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
            task,
            execution: execution.to_string(),
            commands: vec!["cargo test a".into()],
            started_at: super::super::model::now_secs(),
            timeout_secs: 600,
            cancel: super::super::launch::new_cancel_flag(),
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
            task: 1,
            execution: exec,
            commands: vec!["cargo test a".into()],
            // 時限 + 余白より前に始まっている = もう見切ってよい。
            started_at: super::super::model::now_secs()
                .saturating_sub(60 + WATCHDOG_SLACK_SECS + 10),
            timeout_secs: 60,
            cancel: cancel.clone(),
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

    #[test]
    fn スレッドローカルの状態は独立して触れる() {
        with_panel(|p| p.notice = "hello".into());
        with_panel(|p| assert_eq!(p.notice, "hello"));
        with_panel(|p| p.notice.clear());
    }
}
