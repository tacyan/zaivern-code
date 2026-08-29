//! Team Runtime — **desired state と actual state を突き合わせる調停ループ。**
//!
//! ## この層が egui を知らないこと
//!
//! 描画中にプロセスを起こしたりファイルを書いたりすると、フレームが止まり、
//! しかも「毎フレーム同じことをやり直す」事故が起きる。だから Runtime は
//! **[`TeamEffect`] を返すだけ**で、実行は呼び出し側 (app の安全な場所) が行う。
//! 逆向きの入力は [`Observation`] と [`TeamAction`] の 2 本だけ。
//!
//! ```text
//!   Observation ─┐                   ┌─→ TeamEffect (app が実行)
//!                ├→  TeamRuntime  ──┤
//!   TeamAction ──┘                   └─→ TeamSnapshot (GUI が描く)
//! ```
//!
//! ## 冪等性
//!
//! 毎 tick で同じ起動要求・同じ指示を再送しないため、Effect は必ず
//! [`TeamEffect::key`] を持ち、一度処理したキーは記録する。**記録は永続化
//! される**ので、再起動しても同じ指示を撃ち直さない。
//!
//! ## 既存の安全制御を迂回しない
//!
//! 割り当ては必ず [`crate::coordinator::Coordinator::try_assign`] を通す。
//! `scheduler` が「行ける」と言っても、`coordinator` が断ったら**配らない**。
//! ファイルの重なり・前任者の停止未確認・再試行上限は既存側の判断に従う。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::time::Instant;

use crate::coordinator::{self, Coordinator, SessionState};

use super::graph;
use super::model::*;
use super::persistence::{EffectRecord, EffectState, RunDoc, Saved, SCHEMA_VERSION};
use super::plan_schema::TeamPlan;
use super::result_parser::{self as rp, ReportedStatus};
use super::reviewer;
use super::scheduler::{self, Candidate};
use super::state_machine as sm;

/// Activity Feed に残すイベント数の上限。
pub const EVENT_CAP: usize = 500;
/// Effect の記憶数の上限。
///
/// **刈り取ってよいのは成功済みだけ。** 未完了 (`Dispatched`) を落とすと、
/// 実行中の Effect が「知らないもの」に戻って二重に発行される。
pub const EFFECT_KEY_CAP: usize = 2_000;
/// 再試行の既定上限。
pub const DEFAULT_MAX_ATTEMPTS: u8 = 3;

// ── 入力 ─────────────────────────────────────────────────────────────

/// 1 セッションの観測結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionObs {
    pub id: SessionId,
    pub title: String,
    pub provider: String,
    /// 既存の [`crate::app`] が導出した調停層の状態。**ここが真実。**
    pub state: SessionState,
    /// **前回 tick 以降に増えた画面テキスト**。全履歴を渡さないこと
    /// (毎フレーム全部を解析すると、64 体でフレームが止まる)。
    pub text: String,
}

/// tick へ渡す観測。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    pub now: u64,
    pub sessions: Vec<SessionObs>,
}

// ── 出力 ─────────────────────────────────────────────────────────────

/// エージェントを 1 体起こす要求。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentLaunchSpec {
    pub agent_id: AgentId,
    pub name: String,
    pub role: TeamRole,
    pub team_id: TeamId,
    pub workspace_root: PathBuf,
}

/// 検証コマンドを走らせる要求。**コマンドは語に分けて渡す**
/// (シェル文字列として連結しない)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationSpec {
    pub task: TaskId,
    /// 許可リストを通ったコマンド行 (実行側で語に割る)。
    pub commands: Vec<String>,
    pub cwd: PathBuf,
}

/// Runtime が「やってほしい」こと。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TeamEffect {
    StartAgent(AgentLaunchSpec),
    SendInstruction {
        session: SessionId,
        text: String,
        /// 冪等キー。同じキーの指示は二度と出ない。
        key: String,
    },
    StopAgent(SessionId),
    RunValidation(ValidationSpec),
    RequestHumanApproval(Decision),
    PersistState,
}

impl TeamEffect {
    /// 冪等キー。**同じキーの Effect は 1 回しか出さない。**
    pub fn key(&self) -> String {
        match self {
            TeamEffect::StartAgent(s) => format!("start:{}", s.agent_id),
            TeamEffect::SendInstruction { key, .. } => key.clone(),
            TeamEffect::StopAgent(s) => format!("stop:{s}"),
            TeamEffect::RunValidation(v) => format!("validate:{}", v.task),
            TeamEffect::RequestHumanApproval(d) => format!("decide:{}", d.idempotency_key),
            // 保存だけは毎回出してよい (内容が変わるため)。
            TeamEffect::PersistState => String::new(),
        }
    }
}

/// GUI / CLI からの操作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TeamAction {
    /// 計画を承認して開始する。
    Start,
    Pause,
    Resume,
    /// 新規割り当てを止める。実行中エージェントの kill は**承認ゲート**を通す。
    Stop,
    /// 停止の承認が下りた。
    ApproveDecision(EventId),
    RejectDecision(EventId),
    /// 人手が要る状態からもう一度回す。
    RetryTask(TaskId),
    /// 担当を外して配り直す。
    ReassignTask(TaskId),
    /// タスクへ追加の指示を足す。
    AddContext {
        task: TaskId,
        text: String,
    },
}

// ── 本体 ─────────────────────────────────────────────────────────────

/// Team Run 1 本ぶんの状態。
pub struct TeamRuntime {
    goal: TeamGoal,
    teams: Vec<TeamGroup>,
    tasks: Vec<TeamTask>,
    agents: Vec<TeamAgent>,
    events: VecDeque<TeamEvent>,
    decisions: Vec<Decision>,
    run: RunDoc,
    workspace: PathBuf,
    next_event_id: EventId,
    next_task_id: TaskId,
    /// Effect の進み具合 (キー → 記録)。**発行 = 完了ではない。**
    effects: BTreeMap<String, EffectRecord>,
    /// 成功済みを古い順に刈り取るための並び。
    effect_order: VecDeque<String>,
    /// 既存の調停層。**割り当ての最終判断はここ。**
    co: Coordinator,
    /// 登録済みセッション。
    registered: BTreeSet<SessionId>,
    /// 保存が要るか。
    dirty: bool,
}

impl TeamRuntime {
    /// 計画から新しい Run を作る。
    pub fn from_plan(plan: TeamPlan, workspace: PathBuf, opts: RunOptions) -> Self {
        let now = now_secs();
        let next_task_id = plan.tasks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        let mut rt = Self {
            goal: TeamGoal {
                status: GoalStatus::Ready,
                ..plan.goal
            },
            teams: plan.teams,
            tasks: plan.tasks,
            agents: Vec::new(),
            events: VecDeque::new(),
            decisions: Vec::new(),
            run: RunDoc {
                version: SCHEMA_VERSION,
                run_id: opts.run_id.clone(),
                workspace: workspace.display().to_string(),
                spec_source: opts.spec_source.clone(),
                agent_count: opts.agent_count,
                max_attempts: opts.max_attempts,
                review_required: opts.review_required,
                paused: false,
                stopped: false,
                started_at: now,
                updated_at: now,
                effects: Vec::new(),
                done_effects: Vec::new(),
            },
            workspace,
            next_event_id: 1,
            next_task_id,
            effects: BTreeMap::new(),
            effect_order: VecDeque::new(),
            co: Coordinator::new(),
            registered: BTreeSet::new(),
            dirty: true,
        };
        rt.plan_roster();
        rt.log(
            TeamEventKind::PlanReady,
            None,
            None,
            format!(
                "計画を作成しました (タスク {} 件 / 最大 {} 体)",
                rt.tasks.len(),
                rt.run.agent_count
            ),
        );
        rt
    }

    /// Goal の表題を差し替える (フォームの「Goal 名」)。
    ///
    /// **計画そのものは変えない。** 表題は人が読むためのものなので、
    /// Task Graph にも Definition of Done にも影響させない。
    pub fn rename_goal(&mut self, title: &str) {
        let t = title.trim();
        if t.is_empty() {
            return;
        }
        self.goal.title = clamp_text(t);
        self.goal.updated_at = now_secs();
        self.dirty = true;
    }

    /// 保存された状態から復元する。
    ///
    /// **Running / Assigned だったタスクを無条件に Running へ戻さない。**
    /// プロセスが生きているかは復元時点では分からないので、いったん
    /// `Ready` へ落とし、担当が確認できたときだけ再び進める。
    pub fn restore(saved: Saved, workspace: PathBuf) -> Self {
        let next_task_id = saved.tasks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        let next_event_id = saved.events.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        let mut tasks = saved.tasks;
        for t in &mut tasks {
            // セッションはどれも生き残っていない。結び付きは必ず外す。
            t.assigned_session = None;
            t.coordinator_task = None;
            match t.state {
                // **担当が居ないと進まない状態**は空けて `Ready` へ戻す。
                // 「プロセスが生きているはず」で再開すると、居ないものを
                // 待ち続けることになる。
                TeamTaskState::Assigned | TeamTaskState::Running => {
                    t.assigned_agent = None;
                    t.reassign_pending = false;
                    t.state = if t.attempts >= saved.run.max_attempts {
                        TeamTaskState::NeedsUser
                    } else {
                        TeamTaskState::Ready
                    };
                    t.validation.running = false;
                    t.review.running = false;
                }
                // **検証は Zaivern 自身が走らせるので、担当が居なくても再開
                // できる。** ここを `Ready` へ落とすと、出来上がっている成果を
                // 捨ててもう一度実装させることになる。走っていた実行は
                // プロセスと一緒に消えているので、`running` だけ戻して
                // `advance` にもう一度発行させる。
                TeamTaskState::Validating => {
                    t.validation.running = false;
                }
                // レビュー待ちの本体はそのまま。レビュータスク自体は
                // `Assigned` / `Running` なので上の枝で `Ready` へ戻り、
                // 別のセッションへ配り直される。
                TeamTaskState::Reviewing => {
                    t.validation.running = false;
                }
                _ => {}
            }
        }
        let mut agents = saved.agents;
        for a in &mut agents {
            // セッションの生存は次の観測で決まる。いったん未確認にする。
            a.session_id = None;
            a.current_task = None;
            a.state = AgentWorkState::Unknown;
        }
        // **成功済みだけを引き継ぐ。** `Dispatched` は「渡したが成功の返事が
        // 来ていない」= 実行される前に落ちたかもしれないので、記録ごと捨てて
        // **もう一度発行させる**。二重実行にならないのは、各 Effect が
        // 個別に冪等だから (`TeamEffect::key` の doc を参照)。
        //
        // **成功済みでも回収するものが 1 つある: 走っている途中の検証。**
        // 実行側は「裏で走らせ始めた」時点で成功を返す (結果は
        // `note_validation` が別途戻す) ので、結果が戻る前に落ちると
        // `Completed` の記録だけが残る。その実行はプロセスごと消えているのに
        // 記録が新しい発行を止めるので、タスクは `Validating` のまま
        // **永久に止まる**。まだ決着していない検証の記録は引き継がない。
        let unsettled: BTreeSet<String> = tasks
            .iter()
            .filter(|t| t.state == TeamTaskState::Validating)
            .map(|t| format!("validate:{}", t.id))
            .collect();
        let mut effects: BTreeMap<String, EffectRecord> = BTreeMap::new();
        let mut effect_order = VecDeque::new();
        for r in &saved.run.effects {
            if r.state != EffectState::Completed || unsettled.contains(&r.key) {
                continue;
            }
            if effects.insert(r.key.clone(), r.clone()).is_none() {
                effect_order.push_back(r.key.clone());
            }
        }
        let mut rt = Self {
            goal: saved.goal,
            teams: saved.teams,
            tasks,
            agents,
            events: saved.events.into_iter().collect(),
            decisions: saved.decisions,
            run: saved.run,
            workspace,
            next_event_id,
            next_task_id,
            effects,
            effect_order,
            co: Coordinator::new(),
            registered: BTreeSet::new(),
            dirty: false,
        };
        while rt.events.len() > EVENT_CAP {
            rt.events.pop_front();
        }
        rt
    }

    // ── 参照 ──

    pub fn goal(&self) -> &TeamGoal {
        &self.goal
    }
    pub fn teams(&self) -> &[TeamGroup] {
        &self.teams
    }
    pub fn tasks(&self) -> &[TeamTask] {
        &self.tasks
    }
    pub fn agents(&self) -> &[TeamAgent] {
        &self.agents
    }
    pub fn events(&self) -> impl DoubleEndedIterator<Item = &TeamEvent> {
        self.events.iter()
    }
    pub fn decisions(&self) -> &[Decision] {
        &self.decisions
    }
    pub fn run(&self) -> &RunDoc {
        &self.run
    }
    pub fn is_paused(&self) -> bool {
        self.run.paused
    }
    pub fn is_stopped(&self) -> bool {
        self.run.stopped
    }
    pub fn task(&self, id: TaskId) -> Option<&TeamTask> {
        self.tasks.iter().find(|t| t.id == id)
    }
    pub fn agent(&self, id: &AgentId) -> Option<&TeamAgent> {
        self.agents.iter().find(|a| a.id == *id)
    }

    // ── テストと ACK のための入口 (この版ではまだ「発行 = 完了」のまま) ──

    /// **Effect の実行が成功した**と実行側から伝える。
    ///
    /// ここで初めて「済んだ」ことになり、永続化されて再発行されなくなる。
    ///
    /// ## 「済んだ」が意味するものは Effect ごとに違う
    ///
    /// * `SendInstruction` / `RunValidation` — **結果が残る**。届いた指示も、
    ///   記録した実測も再起動をまたいで有効なので、二度と出さない。
    /// * `StartAgent` — 成果は**生きているセッション**そのもの。再起動すれば
    ///   子プロセスは死んでいるので、ACK 済みでも起動し直す必要がある。
    ///   その判断は [`ensure_agents`](Self::ensure_agents) が
    ///   「結び付いたセッションがあるか」で行う (記録だけを見ない)。
    /// * `StopAgent` — セッション ID は再起動で意味を失う。
    /// * `RequestHumanApproval` — `decisions` 側が `idempotency_key` で守る。
    pub fn note_effect_done(&mut self, key: &str) {
        if key.is_empty() {
            return;
        }
        let now = now_secs();
        match self.effects.get_mut(key) {
            Some(r) => {
                r.state = EffectState::Completed;
                r.at = now;
            }
            None => {
                // 発行の記録が無いのに成功だけ来た (刈り取り後など)。
                // 完了として残しておけば、少なくとも再発行はされない。
                self.effects.insert(
                    key.to_string(),
                    EffectRecord {
                        key: key.to_string(),
                        state: EffectState::Completed,
                        at: now,
                    },
                );
                self.effect_order.push_back(key.to_string());
            }
        }
        self.prune_effects();
        self.dirty = true;
    }

    /// **Effect の実行に失敗した**と伝える。記録ごと捨てて再試行できるようにする。
    pub fn note_effect_failed(&mut self, key: &str) {
        if self.effects.remove(key).is_some() {
            self.effect_order.retain(|k| k != key);
            self.dirty = true;
        }
    }

    /// その Effect は成功済みか。
    pub fn effect_completed(&self, key: &str) -> bool {
        self.effects
            .get(key)
            .is_some_and(|r| r.state == EffectState::Completed)
    }

    /// **成功済みだけを古い順に刈り取る。**
    ///
    /// 未完了を落とすと、実行中の Effect が「知らないもの」に戻って
    /// 二重に発行される。
    fn prune_effects(&mut self) {
        while self.effects.len() > EFFECT_KEY_CAP {
            let Some(pos) = self
                .effect_order
                .iter()
                .position(|k| self.effects.get(k).is_some_and(|r| r.state == EffectState::Completed))
            else {
                // 全部が未完了。**1 件も落とさない** (落とすと二重実行になる)。
                break;
            };
            if let Some(k) = self.effect_order.remove(pos) {
                self.effects.remove(&k);
            }
        }
    }

    /// テスト用: 発行済みとして記録する。
    #[cfg(test)]
    pub fn note_effect_dispatched_for_test(&mut self, key: &str) {
        let now = now_secs();
        if self
            .effects
            .insert(
                key.to_string(),
                EffectRecord {
                    key: key.to_string(),
                    state: EffectState::Dispatched,
                    at: now,
                },
            )
            .is_none()
        {
            self.effect_order.push_back(key.to_string());
        }
    }

    /// テスト用: 状態を直に置く (状態機械の検査そのものを書くため)。
    #[cfg(test)]
    pub fn set_state_for_test(&mut self, task: TaskId, state: TeamTaskState) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task) {
            t.state = state;
        }
    }

    /// テスト用: 検証コマンドを差し替える。
    #[cfg(test)]
    pub fn set_validation_commands_for_test(&mut self, task: TaskId, cmds: &[&str]) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task) {
            t.validation_commands = cmds.iter().map(|s| s.to_string()).collect();
        }
    }

    /// 保存用のまとまり。
    pub fn to_saved(&self) -> Saved {
        Saved {
            run: RunDoc {
                effects: self
                    .effect_order
                    .iter()
                    .filter_map(|k| self.effects.get(k).cloned())
                    .collect(),
                done_effects: Vec::new(),
                updated_at: now_secs(),
                ..self.run.clone()
            },
            goal: self.goal.clone(),
            teams: self.teams.clone(),
            tasks: self.tasks.clone(),
            agents: self.agents.clone(),
            decisions: self.decisions.clone(),
            events: self.events.iter().cloned().collect(),
        }
    }

    // ── 起動要求と結び付け ──

    /// 起動したセッションをエージェントへ結び付ける。
    pub fn bind_session(&mut self, agent: &AgentId, session: SessionId) {
        if let Some(a) = self.agents.iter_mut().find(|a| a.id == *agent) {
            a.session_id = Some(session);
            a.last_activity_at = now_secs();
            a.state = AgentWorkState::Idle;
        }
        self.dirty = true;
    }

    /// 起動に失敗した。次の tick でもう一度試せるよう、冪等キーを外す。
    pub fn note_launch_failed(&mut self, agent: &AgentId, why: &str) {
        let key = format!("start:{agent}");
        self.note_effect_failed(&key);
        self.log(
            TeamEventKind::AgentFailed,
            Some(agent.clone()),
            None,
            format!("エージェント {agent} を起動できませんでした: {why}"),
        );
    }

    /// **Zaivern 自身が走らせた検証の結果**を受け取る (app 側の実行器から)。
    ///
    /// ここへ入るものだけが正式な検証証跡になる。エージェントの自己申告は
    /// [`TeamTask::reported_validation`] に分けてあり、ここには入らない。
    pub fn note_validation(&mut self, task: TaskId, runs: Vec<ValidationRun>) {
        let Some(t) = self.tasks.iter_mut().find(|t| t.id == task) else {
            return;
        };
        t.validation.running = false;
        for r in runs {
            t.validation.runs.retain(|x| x.command != r.command);
            t.validation.runs.push(r);
        }
        t.updated_at = now_secs();
        self.dirty = true;
        // 検証の Effect は 1 回の実行で完結する。差し戻し後のやり直しを
        // 発行できるよう、記録を外す。
        self.note_effect_failed(&format!("validate:{task}"));
        let passed = self
            .task(task)
            .map(|t| t.validation.passed(&t.validation_commands))
            .unwrap_or(false);
        self.log(
            TeamEventKind::ValidationCompleted,
            None,
            None,
            format!(
                "#{task} の検証が{}",
                if passed {
                    "成功しました"
                } else {
                    "失敗しました"
                }
            ),
        );
        self.settle_validation(task);
    }

    /// **検証の決着をつける唯一の場所。**
    ///
    /// 実測 ([`ValidationState::runs`]) だけを見て、Reviewing へ進めるか、
    /// 差し戻すか、人へ上げるかを決める。**ここを通らずに `Reviewing` /
    /// `Completed` へ行く経路を作らないこと** — 作った瞬間に「エージェントが
    /// 完了と言っただけで完了になる」が戻ってくる。
    fn settle_validation(&mut self, task: TaskId) {
        let Some(t) = self.task(task).cloned() else {
            return;
        };
        if t.state != TeamTaskState::Validating || t.validation.running {
            return;
        }
        if !t.validation.settled(&t.validation_commands) {
            // 決着していない。失敗が出ているなら差し戻し、まだ結果が
            // 揃っていないだけなら待つ (次の `note_validation` で決まる)。
            if t.validation.failed() {
                self.fail_validation(task);
            }
            return;
        }

        // ── ここから先は「実測で決着がついた」場合だけ ──
        let review_required = self.run.review_required;
        if review_required && t.review_of.is_none() {
            let rev = self.new_review_task(&t);
            let rid = rev.id;
            if let Some(x) = self.tasks.iter_mut().find(|x| x.id == task) {
                x.state = sm::apply(x.state, TeamTaskState::Reviewing).unwrap_or(x.state);
                x.review.running = true;
                x.review.verdict = None;
                x.review.findings.clear();
            }
            self.tasks.push(rev);
            self.log(
                TeamEventKind::ReviewStarted,
                None,
                None,
                format!("#{task} の検証が通ったのでレビュー (#{rid}) を作成しました"),
            );
        } else {
            // レビュー不要 (または自分がレビュータスク)。検証が通ったので完了。
            if let Some(x) = self.tasks.iter_mut().find(|x| x.id == task) {
                x.state = sm::apply(x.state, TeamTaskState::Reviewing).unwrap_or(x.state);
                x.state = sm::apply(x.state, TeamTaskState::Completed).unwrap_or(x.state);
            }
            let owner = self
                .task(task)
                .and_then(|x| x.assigned_agent.clone())
                .unwrap_or_else(|| AgentId::new("(未割り当て)"));
            self.complete_task(task, &owner);
        }
        self.dirty = true;
    }

    /// 検証が失敗した。**Reviewing へは進めず**、差し戻すか人へ上げる。
    fn fail_validation(&mut self, task: TaskId) {
        let max = self.run.max_attempts;
        let mut escalate = false;
        let failed: Vec<String> = self
            .task(task)
            .map(|t| {
                t.validation
                    .runs
                    .iter()
                    .filter(|r| r.exit_code != 0)
                    .map(|r| r.command.clone())
                    .collect()
            })
            .unwrap_or_default();
        // 申告と実測が食い違ったなら、それも次の担当へ渡す。
        let lied: Vec<String> = self
            .task(task)
            .map(|t| {
                t.reported_validation
                    .iter()
                    .filter(|r| r.exit_code == 0 && failed.contains(&r.command))
                    .map(|r| r.command.clone())
                    .collect()
            })
            .unwrap_or_default();

        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task) {
            t.attempts = t.attempts.saturating_add(1);
            t.context.push(clamp_text(&format!(
                "検証が失敗しました: {}",
                failed.join(", ")
            )));
            if !lied.is_empty() {
                t.context.push(clamp_text(&format!(
                    "前回の報告では成功と書かれていましたが、実際には失敗しました: {}",
                    lied.join(", ")
                )));
            }
            t.context = clamp_list(std::mem::take(&mut t.context));
            t.state = sm::apply(t.state, TeamTaskState::Failed).unwrap_or(t.state);
            if t.attempts >= max {
                t.state = sm::apply(t.state, TeamTaskState::NeedsUser).unwrap_or(t.state);
                escalate = true;
            }
        }
        if !escalate {
            // 担当は自分から「終わった」と言って手を離しているので、前任の
            // 停止は確認済みとして既存調停層へ伝えてよい
            // (`release_after_self_report` の doc を参照)。
            self.release_after_self_report(task);
            if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task) {
                t.assigned_agent = None;
                t.assigned_session = None;
                t.state = sm::apply(t.state, TeamTaskState::Ready).unwrap_or(t.state);
                // **失敗した実測はここでは消さない。** 画面と次の担当が
                // 「何が落ちたか」を読むための証跡なので、次に配るときまで残す
                // (`dispatch` が配る瞬間に捨てる)。
            }
        }
        self.log(
            TeamEventKind::ValidationCompleted,
            None,
            None,
            format!("#{task} の検証が失敗したので差し戻しました"),
        );
        if escalate {
            self.raise(
                DecisionKind::AttemptsExhausted,
                Some(task),
                None,
                format!("#{task} の検証が上限回数まで失敗しました"),
                format!("失敗したコマンド: {}", failed.join(", ")),
                vec!["retry".into(), "reassign".into(), "reject".into()],
            );
        }
        self.dirty = true;
    }

    // ── 操作 ──

    /// 人の操作を適用する。返す Effect は tick と同じ扱い。
    pub fn apply_action(&mut self, act: TeamAction) -> Vec<TeamEffect> {
        let mut out = Vec::new();
        match act {
            TeamAction::Start => {
                if self.goal.status == GoalStatus::Ready || self.goal.status == GoalStatus::Planning
                {
                    self.goal.status = GoalStatus::Running;
                    self.run.paused = false;
                    self.run.stopped = false;
                    self.log(
                        TeamEventKind::RunStarted,
                        None,
                        None,
                        "Team Run を開始しました".into(),
                    );
                }
            }
            TeamAction::Pause => {
                if !self.run.paused {
                    self.run.paused = true;
                    self.goal.status = GoalStatus::Paused;
                    self.log(
                        TeamEventKind::RunPaused,
                        None,
                        None,
                        "一時停止しました (新規割り当てのみ停止)".into(),
                    );
                }
            }
            TeamAction::Resume => {
                if self.run.paused {
                    self.run.paused = false;
                    self.goal.status = GoalStatus::Running;
                    self.log(TeamEventKind::RunResumed, None, None, "再開しました".into());
                }
            }
            TeamAction::Stop => {
                // 新規割り当ては即座に止める。**kill は承認ゲートを通す。**
                self.run.stopped = true;
                self.run.paused = true;
                let live: Vec<SessionId> =
                    self.agents.iter().filter_map(|a| a.session_id).collect();
                self.log(
                    TeamEventKind::RunStopped,
                    None,
                    None,
                    "新規割り当てを停止しました".into(),
                );
                if !live.is_empty() {
                    let d = self.make_decision(
                        DecisionKind::StopAgents,
                        None,
                        None,
                        format!("実行中のエージェント {} 体を停止しますか", live.len()),
                        "停止すると、進行中の作業は失われる可能性があります".into(),
                        vec!["approve".into(), "reject".into()],
                        format!("stop-agents:{}", self.run.run_id),
                    );
                    if let Some(d) = d {
                        out.push(TeamEffect::RequestHumanApproval(d));
                    }
                }
            }
            TeamAction::ApproveDecision(id) => {
                if let Some(pos) = self.decisions.iter().position(|d| d.id == id) {
                    let d = self.decisions.remove(pos);
                    self.log(
                        TeamEventKind::DecisionResolved,
                        None,
                        None,
                        format!("承認しました: {}", d.reason),
                    );
                    if d.kind == DecisionKind::StopAgents {
                        match d.task_id {
                            // Reassign — そのタスクの担当だけを止める。
                            Some(tid) => match self.live_session_of(tid) {
                                Some(s) => out.push(TeamEffect::StopAgent(s)),
                                // 承認するまでの間に消えていた。そのまま回収する。
                                None => self.free_task(tid, false),
                            },
                            // Stop Team — 実行中を全部止める。
                            None => {
                                for s in self.agents.iter().filter_map(|a| a.session_id) {
                                    out.push(TeamEffect::StopAgent(s));
                                }
                            }
                        }
                    }
                }
            }
            TeamAction::RejectDecision(id) => {
                if let Some(pos) = self.decisions.iter().position(|d| d.id == id) {
                    let d = self.decisions.remove(pos);
                    self.log(
                        TeamEventKind::DecisionResolved,
                        None,
                        None,
                        format!("却下しました: {}", d.reason),
                    );
                    if d.kind == DecisionKind::StopAgents {
                        match d.task_id {
                            // Reassign の拒否 — **元の担当も状態も壊さない。**
                            Some(tid) => {
                                if let Some(t) = self.tasks.iter_mut().find(|t| t.id == tid) {
                                    t.reassign_pending = false;
                                    t.updated_at = now_secs();
                                }
                            }
                            // Stop Team の拒否 — 止めた割り当てを戻す。
                            None => {
                                self.run.stopped = false;
                                self.run.paused = false;
                            }
                        }
                    }
                }
            }
            TeamAction::RetryTask(id) => {
                let max = self.run.max_attempts;
                // **ここで `confirm_stopped` は呼ばない。** Retry が動くのは
                // `NeedsUser` / `Failed` のときだけで、どちらもその手前で
                // 担当を解放済み。呼ぶと「人が押した = 停止済み」という
                // 誤った前提を持ち込むことになる。
                let mut retried = false;
                if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
                    if t.state == TeamTaskState::NeedsUser || t.state == TeamTaskState::Failed {
                        // 人が回すときは試行回数を 1 つ戻す (無限には回さない)。
                        t.attempts = t.attempts.min(max.saturating_sub(1));
                        // **`NeedsUser` から出られるのは人の操作だけ**なので、
                        // ここは意図的に表を迂回する (`sm::force` の存在理由)。
                        // 自動処理からこの行へ来る経路は無い —
                        // `state_machine::tests::人へはどこからでも上げられるが完了からは上げない`
                        // と `runtime_tests::人の操作だけがneeds_userから戻せる` が番人。
                        t.state = sm::force(t.state, TeamTaskState::Ready);
                        t.assigned_agent = None;
                        t.assigned_session = None;
                        t.reassign_pending = false;
                        t.updated_at = now_secs();
                        retried = true;
                    }
                }
                // **効かなかった Retry で判断を消さない。** 実行中のタスクへ
                // 撃っても状態は変わらないので、ここで承認待ちの停止要求まで
                // 消すと「停止待ちの印だけが残り、承認する手段が画面から
                // 消える」タスクができる (`runtime_tests::効かなかったretryは
                // 停止承認を消さない`)。
                if retried {
                    self.decisions.retain(|d| d.task_id != Some(id));
                }
                self.dirty = true;
            }
            TeamAction::ReassignTask(id) => {
                // **旧担当が生きているうちは配り直さない。**
                //
                // 担当を外して `Ready` に戻すと、まだ編集しているかもしれない
                // 旧担当と、新しい担当が同じファイルを同時に持つ。既存の
                // 重なり判定は「占有しているタスク」を見るので、解放した
                // 瞬間にすり抜ける。
                match self.live_session_of(id) {
                    None => {
                        // 担当が居ない (未割り当て / 既に消えた) ので安全に回収できる。
                        self.free_task(id, false);
                    }
                    Some(_) => {
                        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
                            t.reassign_pending = true;
                            t.updated_at = now_secs();
                        }
                        let d = self.make_decision(
                            DecisionKind::StopAgents,
                            Some(id),
                            None,
                            format!("#{id} の担当を替えるため、いまの担当を停止しますか"),
                            "停止するまで配り直しません (同じファイルを 2 人が同時に持たないため)"
                                .into(),
                            vec!["approve".into(), "reject".into()],
                            format!("reassign-stop:{id}"),
                        );
                        if let Some(d) = d {
                            out.push(TeamEffect::RequestHumanApproval(d));
                        }
                    }
                }
                self.dirty = true;
            }
            TeamAction::AddContext { task, text } => {
                if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task) {
                    t.context.push(clamp_text(&text));
                    t.context = clamp_list(std::mem::take(&mut t.context));
                    t.updated_at = now_secs();
                }
                self.dirty = true;
            }
        }
        self.dirty = true;
        out.push(TeamEffect::PersistState);
        self.dispatch_effects(out)
    }

    // ── 調停ループ ──

    /// 1 tick。**同じ入力で同じ Effect を返す** (時刻以外)。
    pub fn tick(&mut self, obs: &Observation) -> Vec<TeamEffect> {
        let mut out = Vec::new();

        // 1) 観測 — セッションの状態をエージェントへ写す。
        self.sync_sessions(obs);

        // 2) 報告の取り込み。**Pause 中でも読む** (状態更新は続ける)。
        self.harvest(obs);

        // 3) 停止待ちの Reassign を決着させる (再起動をまたいでも進む)。
        self.settle_reassign();

        // 4) 依存が済んだタスクを Ready にする。
        self.promote_ready();

        // 5) 進んだタスクを先へ (検証 → レビュー → 完了)。
        self.advance(&mut out);

        // 6) Pause / Stop 中は**新規割り当てをしない**。
        if self.accepting_work() {
            self.ensure_agents(&mut out);
            self.dispatch(&mut out);
        }

        // 7) Goal の状態を更新する。
        self.update_goal();

        if self.dirty {
            out.push(TeamEffect::PersistState);
            self.dirty = false;
        }
        self.dispatch_effects(out)
    }

    /// 新しい仕事を配ってよい状態か。
    ///
    /// **`GoalStatus::Running` だけを見てはいけない。** Goal の状態は
    /// 表示用に `Reviewing` / `Integrating` / `NeedsUser` へも動くので、
    /// そこを条件にすると「レビュー待ちが 1 本あるだけで新しい割り当てが
    /// 全部止まる」= レビュー担当さえ配れずに永久に止まる (実測で詰まった)。
    /// 止めるのは**人が止めたとき**と**まだ始まっていない / もう終わった
    /// とき**だけ。
    fn accepting_work(&self) -> bool {
        !self.run.paused
            && !self.run.stopped
            && !matches!(
                self.goal.status,
                GoalStatus::Planning
                    | GoalStatus::Ready
                    | GoalStatus::Paused
                    | GoalStatus::Completed
                    | GoalStatus::Failed
            )
    }

    /// まだ出していない Effect だけを残し、**発行済み**として記録する。
    ///
    /// **ここでは「完了」にしない。** 実行側が成功を返して
    /// [`note_effect_done`](Self::note_effect_done) を呼んだときだけ完了になる。
    /// 発行と実行の間で落ちた Effect は、次の起動で記録ごと捨てられて
    /// もう一度発行される。
    fn dispatch_effects(&mut self, effects: Vec<TeamEffect>) -> Vec<TeamEffect> {
        let mut out = Vec::new();
        let now = now_secs();
        for e in effects {
            let k = e.key();
            if k.is_empty() {
                // PersistState は毎回出してよいが、1 回にまとめる。
                if !out.iter().any(|x| matches!(x, TeamEffect::PersistState)) {
                    out.push(e);
                }
                continue;
            }
            // 発行済み (返事待ち) も、成功済みも、もう一度は出さない。
            if self.effects.contains_key(&k) {
                continue;
            }
            self.effects.insert(
                k.clone(),
                EffectRecord {
                    key: k.clone(),
                    state: EffectState::Dispatched,
                    at: now,
                },
            );
            self.effect_order.push_back(k);
            self.prune_effects();
            out.push(e);
        }
        out
    }

    /// セッションの状態をエージェントへ写す。
    fn sync_sessions(&mut self, obs: &Observation) {
        let live: BTreeMap<SessionId, &SessionObs> =
            obs.sessions.iter().map(|s| (s.id, s)).collect();

        // 既存調停層への登録 (未登録のものだけ)。
        for id in live.keys() {
            if self.registered.insert(*id) {
                self.co.register_session(*id);
            }
        }
        let gone: Vec<SessionId> = self
            .registered
            .iter()
            .copied()
            .filter(|id| !live.contains_key(id))
            .collect();
        for id in gone {
            self.registered.remove(&id);
            self.co.unregister_session(id);
        }

        for a in &mut self.agents {
            let Some(sid) = a.session_id else {
                continue;
            };
            match live.get(&sid) {
                Some(s) => {
                    a.provider = s.provider.clone();
                    let task = self.tasks.iter().find(|t| {
                        t.assigned_agent.as_ref() == Some(&a.id) && !t.state.is_terminal()
                    });
                    a.current_task = task.map(|t| t.id);
                    let next = super::roles::derive_agent_work_state(
                        s.state,
                        task,
                        task.map(|t| &t.validation),
                        task.map(|t| &t.review),
                    );
                    if next != a.state {
                        a.state = next;
                        a.last_activity_at = obs.now;
                    }
                    if !s.text.trim().is_empty() {
                        a.last_activity_at = obs.now;
                    }
                }
                None => {
                    // セッションが消えた。**担当を勝手に配り直さない**
                    // (前任者の停止確認は下の release_dead が既存側へ通す)。
                    a.session_id = None;
                    a.state = AgentWorkState::Exited;
                }
            }
        }
        self.release_dead(obs.now);
    }

    /// 消えたセッションが握っていたタスクを解放する。
    ///
    /// **既存調停層の順序を守る**: `note_exited` → `confirm_stopped` →
    /// 次の割り当て。飛ばすと `PreviousHolderNotStopped` で断られる。
    fn release_dead(&mut self, now: u64) {
        let alive: BTreeSet<SessionId> = self.agents.iter().filter_map(|a| a.session_id).collect();
        let orphaned: Vec<(TaskId, SessionId)> = self
            .tasks
            .iter()
            .filter(|t| t.state.is_held())
            .filter_map(|t| t.assigned_session.map(|s| (t.id, s)))
            .filter(|(_, s)| !alive.contains(s))
            .collect();
        if orphaned.is_empty() {
            return;
        }
        let at = Instant::now();
        for (task_id, session) in orphaned {
            // **順序を飛ばさない**: note_exited → confirm_stopped → 解放。
            self.co.note_exited(session, at);
            // 人が Reassign を押して止めたぶんは、失敗として数えない。
            let asked = self.task(task_id).is_some_and(|t| t.reassign_pending);
            self.free_task(task_id, !asked);
            self.log(
                TeamEventKind::TaskFailed,
                None,
                None,
                if asked {
                    format!("#{task_id} の担当が停止したので配り直せます")
                } else {
                    format!("#{task_id} の担当セッションが消えたため回収しました")
                },
            );
        }
        let _ = now;
        self.dirty = true;
    }

    /// 画面テキストから報告を取り込む。
    fn harvest(&mut self, obs: &Observation) {
        for s in &obs.sessions {
            if s.text.trim().is_empty() {
                continue;
            }
            let Some(agent) = self
                .agents
                .iter()
                .find(|a| a.session_id == Some(s.id))
                .map(|a| a.id.clone())
            else {
                continue;
            };
            for body in rp::extract_blocks(&s.text, rp::RESULT_OPEN, rp::RESULT_CLOSE) {
                self.take_result(&agent, &body);
            }
            for body in rp::extract_blocks(&s.text, reviewer::REVIEW_OPEN, reviewer::REVIEW_CLOSE) {
                self.take_review(&agent, &body);
            }
            for body in rp::extract_blocks(&s.text, rp::EVENT_OPEN, rp::EVENT_CLOSE) {
                self.take_event(&agent, &body, obs.now);
            }
        }
    }

    /// 完了報告 1 件。
    fn take_result(&mut self, agent: &AgentId, body: &str) {
        let doc = match rp::parse_result(body) {
            Ok(d) => d,
            Err(e) => {
                self.log(
                    TeamEventKind::Rejected,
                    Some(agent.clone()),
                    None,
                    e.detail(),
                );
                return;
            }
        };
        let task_id = doc.task_id;
        let Some(task) = self.tasks.iter().find(|t| t.id == task_id).cloned() else {
            self.log(
                TeamEventKind::Rejected,
                Some(agent.clone()),
                None,
                format!("報告のタスク #{task_id} は存在しません"),
            );
            return;
        };
        // 担当していないタスクの報告は受け取らない。
        if task.assigned_agent.as_ref() != Some(agent) {
            self.log(
                TeamEventKind::Rejected,
                Some(agent.clone()),
                Some(task_id_as_agent(&task)),
                format!("#{task_id} はこのエージェントの担当ではありません"),
            );
            return;
        }

        match rp::accept(doc, &task) {
            Ok(acc) => self.apply_accepted(agent, acc),
            Err(e) => {
                // **却下は必ず本人へ伝える** (黙って捨てると永久に待つ)。
                self.log(
                    TeamEventKind::Rejected,
                    Some(agent.clone()),
                    None,
                    format!("#{task_id} の完了報告を却下: {}", e.detail()),
                );
                if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task_id) {
                    t.context.push(clamp_text(&format!(
                        "前回の完了報告は却下されました: {}",
                        e.detail()
                    )));
                    t.context = clamp_list(std::mem::take(&mut t.context));
                    t.updated_at = now_secs();
                }
                self.dirty = true;
            }
        }
    }

    fn apply_accepted(&mut self, agent: &AgentId, acc: rp::AcceptedResult) {
        let now = now_secs();
        let max = self.run.max_attempts;
        let mut escalate: Option<(TaskId, String)> = None;
        let mut release_after = false;

        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == acc.task_id) {
            t.last_summary = acc.summary.clone();
            t.changed_files = acc.changed_files.clone();
            t.blockers = acc.blockers.clone();
            t.updated_at = now;
            match acc.status {
                ReportedStatus::Blocked => {
                    t.state = sm::apply(t.state, TeamTaskState::Blocked).unwrap_or(t.state);
                }
                ReportedStatus::Failed => {
                    release_after = true;
                    t.attempts = t.attempts.saturating_add(1);
                    if t.attempts >= max {
                        t.state = sm::apply(t.state, TeamTaskState::NeedsUser).unwrap_or(t.state);
                        escalate = Some((t.id, "実装が上限回数まで失敗しました".to_string()));
                    } else {
                        t.state = sm::apply(t.state, TeamTaskState::Failed).unwrap_or(t.state);
                    }
                }
                ReportedStatus::Completed => {
                    // **自己申告は正式な検証証跡にしない。**
                    //
                    // `"cargo test / exit_code: 0"` は、実際にテストを走らせ
                    // なくても書ける。ここで `validation.runs` へ入れると、
                    // 次の判断が `passed()` を見た瞬間に**申告どおり成功**に
                    // なり、「検証コマンドが実行され、成功しなければ完了に
                    // しない」という中核の保証が空文になる。
                    //
                    // 参考情報としては残す — 実測と食い違ったときに、次の
                    // 担当へ「前回はこう報告されていた」と渡すため。
                    t.reported_validation = acc.validation.clone();
                    // 前回の実測は、今回の実装に対する証跡ではない。捨てる。
                    t.validation.runs.clear();
                    t.validation.running = false;
                    // **表に無い遷移を素通りしない。** `Assigned → Validating`
                    // は表に無いので、`Assigned → Running → Validating` の順で
                    // 通す (割り当て直後に報告が来る筋書きがある)。
                    t.state = sm::apply(t.state, TeamTaskState::Running).unwrap_or(t.state);
                    t.state = sm::apply(t.state, TeamTaskState::Validating).unwrap_or(t.state);
                }
            }
        }

        // **ここでは先へ進めない。** 完了報告が意味するのは「実装が終わったと
        // 本人が言った」までで、そこから Reviewing / Completed へ進むかは
        // Zaivern 自身が走らせた検証の結果 ([`Self::settle_validation`]) が
        // 決める。`advance` が `Validating` のタスクへ `RunValidation` を出し、
        // `note_validation` が実測を受けて決着させる。
        if acc.status == ReportedStatus::Completed {
            self.log(
                TeamEventKind::ValidationStarted,
                Some(agent.clone()),
                None,
                format!("#{} の完了報告を受けました。検証はこちらで実行します", acc.task_id),
            );
        }

        if release_after {
            // 失敗を自分から報告した = もう編集していない。
            self.release_after_self_report(acc.task_id);
            if let Some(t) = self.tasks.iter_mut().find(|t| t.id == acc.task_id) {
                t.assigned_session = None;
                t.assigned_agent = None;
            }
        }

        if let Some((tid, why)) = escalate {
            self.raise(
                DecisionKind::AttemptsExhausted,
                Some(tid),
                None,
                why,
                format!("#{tid} は自動では進められません"),
                vec!["retry".into(), "reassign".into(), "reject".into()],
            );
        }
        self.dirty = true;
    }

    /// レビュー報告 1 件。
    fn take_review(&mut self, agent: &AgentId, body: &str) {
        // このエージェントが担当しているレビュータスクを探す。
        // **終わったレビュータスクを掴まない。** 同じ担当が同じ対象を 2 度
        // レビューする (差し戻し → 再レビュー) と、閉じた 1 本目が先に
        // 見つかって報告が迷子になる (実測で E2E が止まった)。
        let Some(rev) = self
            .tasks
            .iter()
            .find(|t| {
                t.assigned_agent.as_ref() == Some(agent)
                    && t.review_of.is_some()
                    && !t.state.is_terminal()
            })
            .cloned()
        else {
            self.log(
                TeamEventKind::Rejected,
                Some(agent.clone()),
                None,
                "レビュー報告が来ましたが、このエージェントはレビュー担当ではありません".into(),
            );
            return;
        };
        let target_id = rev.review_of.unwrap_or(0);
        let parsed = reviewer::parse_review(body, target_id);
        let acc = match parsed {
            Ok(a) => a,
            Err(e) => {
                self.log(
                    TeamEventKind::Rejected,
                    Some(agent.clone()),
                    None,
                    format!("レビュー報告を却下: {}", e.detail()),
                );
                return;
            }
        };

        let max = self.run.max_attempts;
        let mut escalate = false;
        let mut released = false;
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == target_id) {
            t.review.running = false;
            t.review.reviewer = Some(agent.clone());
            t.review.reviewer_session = rev.assigned_session;
            t.review.verdict = Some(acc.verdict);
            t.review.findings = acc.findings.clone();
            t.updated_at = now_secs();
            match acc.verdict {
                ReviewVerdict::Approve => {
                    t.state = sm::apply(t.state, TeamTaskState::Completed).unwrap_or(t.state);
                }
                ReviewVerdict::RequestChanges => {
                    t.attempts = t.attempts.saturating_add(1);
                    for c in reviewer::findings_as_context(&acc.findings) {
                        t.context.push(c);
                    }
                    t.context = clamp_list(std::mem::take(&mut t.context));
                    t.state =
                        sm::apply(t.state, TeamTaskState::RevisionRequired).unwrap_or(t.state);
                    if t.attempts >= max {
                        t.state = sm::apply(t.state, TeamTaskState::NeedsUser).unwrap_or(t.state);
                        escalate = true;
                    } else {
                        t.state = sm::apply(t.state, TeamTaskState::Ready).unwrap_or(t.state);
                        released = true;
                        t.assigned_agent = None;
                        t.assigned_session = None;
                        // 検証はやり直す。証跡は次に配る瞬間 (`dispatch`) に捨てる。
                    }
                }
            }
        }
        if released {
            // 差し戻しは担当が自分から手を離した状態なので、前任の停止を
            // 確認済みとして既存調停層へ伝える (伝えないと引き渡しを断られ、
            // 差し戻したタスクが二度と配られない — 実測で詰まった)。
            self.release_after_self_report(target_id);
        }
        // レビュータスク自体も**同じ 1 本の経路**で締める。
        //
        // 直接 `Completed` を代入すると `Running → Completed` という表に無い
        // 遷移になる。レビュータスクは `validation_commands` が空なので、
        // `Validating` へ入れれば `settle_validation` が「走らせるものが無い =
        // 決着」として `Reviewing → Completed` まで運ぶ。
        if let Some(r) = self.tasks.iter_mut().find(|t| t.id == rev.id) {
            r.state = sm::apply(r.state, TeamTaskState::Running).unwrap_or(r.state);
            r.state = sm::apply(r.state, TeamTaskState::Validating).unwrap_or(r.state);
            r.updated_at = now_secs();
        }
        self.settle_validation(rev.id);
        self.log(
            TeamEventKind::ReviewCompleted,
            Some(agent.clone()),
            None,
            match acc.verdict {
                ReviewVerdict::Approve => format!("#{target_id} を APPROVE しました"),
                ReviewVerdict::RequestChanges => format!(
                    "#{target_id} に {} 件の指摘 (REQUEST_CHANGES)",
                    acc.findings.len()
                ),
            },
        );
        if acc.verdict == ReviewVerdict::Approve {
            self.complete_task(target_id, agent);
        }
        if escalate {
            self.raise(
                DecisionKind::AttemptsExhausted,
                Some(target_id),
                None,
                format!("#{target_id} が再試行の上限 ({max} 回) に達しました"),
                "指摘が繰り返し解消されていません".into(),
                vec!["retry".into(), "reassign".into(), "reject".into()],
            );
        }
        self.dirty = true;
    }

    /// サブエージェントイベント 1 件。
    fn take_event(&mut self, agent: &AgentId, body: &str, now: u64) {
        let doc = match rp::parse_event(body) {
            Ok(d) => d,
            Err(e) => {
                self.log(
                    TeamEventKind::Rejected,
                    Some(agent.clone()),
                    None,
                    e.detail(),
                );
                return;
            }
        };
        let known: Vec<(AgentId, Option<AgentId>)> = self
            .agents
            .iter()
            .map(|a| (a.id.clone(), a.parent_id.clone()))
            .collect();
        let reporter_task = self
            .tasks
            .iter()
            .find(|t| t.assigned_agent.as_ref() == Some(agent) && !t.state.is_terminal())
            .map(|t| t.id);
        if let Err(e) = rp::check_event(&doc, &known, agent, reporter_task) {
            self.log(
                TeamEventKind::Rejected,
                Some(agent.clone()),
                None,
                e.detail(),
            );
            return;
        }

        if doc.kind.starts_with("sub_agent_") {
            let sub_id = AgentId::new(doc.agent_id.trim());
            let parent = AgentId::new(doc.parent_id.trim());
            let team = self
                .agent(&parent)
                .map(|p| p.team_id.clone())
                .unwrap_or_else(|| TeamId::new("implementation"));
            let action = clamp_text(doc.action.trim());
            let state = match doc.kind.as_str() {
                "sub_agent_blocked" => AgentWorkState::Blocked,
                "sub_agent_completed" => AgentWorkState::Completed,
                "sub_agent_failed" => AgentWorkState::Exited,
                _ => AgentWorkState::Working,
            };
            if let Some(existing) = self.agents.iter_mut().find(|a| a.id == sub_id) {
                existing.state = state;
                existing.current_action = action.clone();
                existing.current_task = doc.task_id;
                existing.last_activity_at = now;
            } else {
                self.agents.push(TeamAgent {
                    id: sub_id.clone(),
                    name: doc.agent_id.trim().to_string(),
                    role: TeamRole::parse(&doc.role),
                    team_id: team,
                    parent_id: Some(parent.clone()),
                    // **報告されただけ。実在するセッションとして描かない。**
                    kind: AgentKind::ReportedSubAgent,
                    session_id: None,
                    provider: String::new(),
                    state,
                    current_task: doc.task_id,
                    current_action: action.clone(),
                    children: Vec::new(),
                    created_at: now,
                    last_activity_at: now,
                });
                if let Some(p) = self.agents.iter_mut().find(|a| a.id == parent) {
                    if !p.children.contains(&sub_id) {
                        p.children.push(sub_id.clone());
                    }
                }
            }
            self.log(
                TeamEventKind::SubAgentReported,
                Some(agent.clone()),
                Some(sub_id),
                format!("{}: {}", doc.kind, action),
            );
        } else {
            // タスク側のイベントは表示だけに使う (状態は報告ブロックで動かす)。
            if let Some(a) = self.agents.iter_mut().find(|a| a.id == *agent) {
                a.current_action = clamp_text(doc.action.trim());
                a.last_activity_at = now;
            }
            self.log(
                TeamEventKind::AgentProgress,
                Some(agent.clone()),
                None,
                format!("{}: {}", doc.kind, doc.action.trim()),
            );
        }
        self.dirty = true;
    }

    /// 依存が済んだタスクを Ready にする。
    fn promote_ready(&mut self) {
        let ready = graph::newly_ready(&self.tasks);
        if ready.is_empty() {
            return;
        }
        for id in ready {
            // 遷移が断られたら**黙らない**。「なぜか Ready にならない」を
            // 追えるように理由をそのまま残す。
            let refused = self
                .tasks
                .iter_mut()
                .find(|t| t.id == id)
                .and_then(|t| match sm::apply(t.state, TeamTaskState::Ready) {
                    Ok(next) => {
                        t.state = next;
                        t.updated_at = now_secs();
                        None
                    }
                    Err(e) => Some(e.detail()),
                });
            match refused {
                Some(why) => self.log(
                    TeamEventKind::TaskBlocked,
                    None,
                    None,
                    format!("#{id} を Ready にできません: {why}"),
                ),
                None => self.log(
                    TeamEventKind::TaskReady,
                    None,
                    None,
                    format!("#{id} の依存が解決しました"),
                ),
            }
        }
        self.dirty = true;
    }

    /// 検証の実行が要るタスクへ Effect を出す。
    ///
    /// **`Validating` のタスクを永久に止めない。** 走らせるものが無いなら
    /// その場で決着させ、走らせてはいけないものが混じっているなら人へ上げる。
    fn advance(&mut self, out: &mut Vec<TeamEffect>) {
        let cwd = self.workspace.clone();
        let pending: Vec<(TaskId, Vec<String>)> = self
            .tasks
            .iter()
            .filter(|t| t.state == TeamTaskState::Validating && !t.validation.running)
            .filter(|t| !t.validation.passed(&t.validation_commands))
            .map(|t| (t.id, t.validation_commands.clone()))
            .collect();
        for (task, commands) in pending {
            if commands.is_empty() {
                // 走らせるものが無い = 検証の決着はここでつく。
                self.settle_validation(task);
                continue;
            }
            // **許可リストを通ったものだけ**を実行側へ渡す。
            let unsafe_cmds: Vec<String> = commands
                .iter()
                .filter(|c| graph::check_command(c).is_err())
                .cloned()
                .collect();
            if !unsafe_cmds.is_empty() {
                // 自動実行しないコマンドが混じっている。**黙って落とすと、
                // そのコマンドは永久に成功せず Validating で止まる。**
                if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task) {
                    t.state = sm::apply(t.state, TeamTaskState::NeedsUser).unwrap_or(t.state);
                    t.updated_at = now_secs();
                }
                self.raise(
                    DecisionKind::DangerousCommand,
                    Some(task),
                    None,
                    format!(
                        "#{task} の検証コマンドは自動実行しません: {}",
                        unsafe_cmds.join(", ")
                    ),
                    "コマンドを直すか、人が実行して結果を戻してください".into(),
                    vec!["retry".into(), "reject".into()],
                );
                continue;
            }
            if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task) {
                t.validation.running = true;
            }
            self.log(
                TeamEventKind::ValidationStarted,
                None,
                None,
                format!("#{task} の検証を開始します"),
            );
            out.push(TeamEffect::RunValidation(ValidationSpec {
                task,
                commands,
                cwd: cwd.clone(),
            }));
        }
    }

    /// 必要なぶんだけエージェントを起こす。**無条件に N 体起こさない。**
    fn ensure_agents(&mut self, out: &mut Vec<TeamEffect>) {
        let want = scheduler::desired_sessions(&self.tasks, self.run.agent_count);
        let bound = self
            .agents
            .iter()
            .filter(|a| a.kind == AgentKind::ManagedSession && a.session_id.is_some())
            .count();
        if bound >= want {
            return;
        }
        // **ACK は返ったのにセッションが結び付いていない**起動要求を拾い直す。
        //
        // 実行側が「起動した」と返してから `bind_session` を呼ぶまでの間に
        // 落ちると、記録は成功のまま・エージェントは居ない、という状態が残る。
        // 記録を外して撃ち直せるようにする (居るなら `session_id` が入って
        // いるので、ここは通らない)。
        let orphan_keys: Vec<String> = self
            .agents
            .iter()
            .filter(|a| a.kind == AgentKind::ManagedSession && a.session_id.is_none())
            .map(|a| format!("start:{}", a.id))
            .filter(|k| self.effect_completed(k))
            .collect();
        for k in orphan_keys {
            self.note_effect_failed(&k);
        }

        let root = self.workspace.clone();
        let specs: Vec<AgentLaunchSpec> = self
            .agents
            .iter()
            .filter(|a| a.kind == AgentKind::ManagedSession && a.session_id.is_none())
            .take(want - bound)
            .map(|a| AgentLaunchSpec {
                agent_id: a.id.clone(),
                name: a.name.clone(),
                role: a.role,
                team_id: a.team_id.clone(),
                workspace_root: root.clone(),
            })
            .collect();
        for s in specs {
            self.log(
                TeamEventKind::AgentStarted,
                Some(s.agent_id.clone()),
                None,
                format!("{} を起動します", s.name),
            );
            out.push(TeamEffect::StartAgent(s));
        }
    }

    /// Ready なタスクを配る。**既存 `coordinator` が断ったら配らない。**
    fn dispatch(&mut self, out: &mut Vec<TeamEffect>) {
        let candidates: Vec<Candidate> = self
            .agents
            .iter()
            .filter(|a| a.kind == AgentKind::ManagedSession)
            .filter_map(|a| {
                let sid = a.session_id?;
                Some(Candidate {
                    agent: a.id.clone(),
                    session: sid,
                    state: work_to_session_state(a.state),
                    caps: vec![a.name.to_ascii_lowercase(), a.role.key().to_string()],
                    // **レビュー待ちは「手が空いている」扱い。** 実装担当が
                    // レビューの間ずっと忙しいことになると、レビュー候補が
                    // 枯れて誰も進めなくなる。
                    holding: self
                        .tasks
                        .iter()
                        .find(|t| t.assigned_agent.as_ref() == Some(&a.id) && t.state.is_working())
                        .map(|t| t.id),
                })
            })
            .collect();
        if candidates.is_empty() {
            return;
        }
        let depth = graph::critical_depth(&self.tasks);
        let plan = scheduler::plan_assignments(&self.tasks, &candidates, &depth);

        for u in &plan.unassigned {
            // 候補が居ないだけなら黙る (次の tick で解決しうる)。
            // 重なりと「本人しか居ない」は人へ上げる価値がある。
            // **どのタスクの話かは必ず添える** (理由だけでは追えない)。
            let subject = u.task();
            match u {
                scheduler::Unassigned::FileOverlap { .. } => {
                    self.raise(
                        DecisionKind::FileScopeOverlap,
                        Some(subject),
                        None,
                        u.detail(),
                        "担当ファイルを分けるか、順番に実行してください".into(),
                        vec!["reassign".into(), "reject".into()],
                    );
                }
                scheduler::Unassigned::ReviewerWouldBeAuthor(_) => {
                    self.raise(
                        DecisionKind::NoCandidate,
                        Some(subject),
                        None,
                        u.detail(),
                        "レビューには実装担当と別のセッションが要ります".into(),
                        vec!["retry".into(), "reject".into()],
                    );
                }
                _ => {}
            }
        }

        let at = Instant::now();
        for a in plan.assignments {
            // 既存調停層へ登録して、そこで最終判断させる。
            let Some(task) = self.tasks.iter().find(|t| t.id == a.task).cloned() else {
                continue;
            };
            let coord_id = match task.coordinator_task {
                Some(id) => id,
                None => {
                    let files: Vec<&str> = task.files.iter().map(|s| s.as_str()).collect();
                    let caps: Vec<&str> = task.required_caps.iter().map(|s| s.as_str()).collect();
                    let id = self.co.add_task_with_files(
                        task.title.clone(),
                        task.description.clone(),
                        &caps,
                        &files,
                        at,
                    );
                    if let Some(t) = self.tasks.iter_mut().find(|t| t.id == a.task) {
                        t.coordinator_task = Some(id);
                    }
                    id
                }
            };
            let infos: Vec<coordinator::SessionInfo> = candidates
                .iter()
                .filter(|c| c.session == a.session)
                .map(|c| c.as_info())
                .collect();
            match self.co.try_assign(coord_id, &infos, at) {
                Ok(session) => {
                    self.co.note_running(coord_id, at);
                    let text = self.instruction_for(&task, &a.agent);
                    if let Some(t) = self.tasks.iter_mut().find(|t| t.id == a.task) {
                        // **前回の実測はこれから作るものへの証跡ではない。**
                        // 配る瞬間に捨てる (1 か所に閉じる)。
                        t.validation.runs.clear();
                        t.validation.running = false;
                        t.reported_validation.clear();
                        t.assigned_agent = Some(a.agent.clone());
                        t.assigned_session = Some(session);
                        t.state = sm::apply(t.state, TeamTaskState::Assigned).unwrap_or(t.state);
                        t.state = sm::apply(t.state, TeamTaskState::Running).unwrap_or(t.state);
                        t.updated_at = now_secs();
                    }
                    self.log(
                        TeamEventKind::TaskAssigned,
                        None,
                        Some(a.agent.clone()),
                        format!("#{} を {} へ割り当てました", a.task, a.agent),
                    );
                    out.push(TeamEffect::SendInstruction {
                        session,
                        text,
                        // **試行回数まで含めて鍵にする。** これが無いと
                        // 差し戻し後の再指示が「同じ指示」として抑止される。
                        key: format!("instr:{}:{}:{}", a.task, a.agent, task.attempts),
                    });
                    self.dirty = true;
                }
                Err(refusal) => {
                    // 既存側が断った。**回避しない。**
                    self.log(
                        TeamEventKind::TaskBlocked,
                        None,
                        None,
                        format!("#{} の割り当てを見送りました: {}", a.task, refusal.label()),
                    );
                    if let coordinator::AssignRefusal::FileOverlap { with, .. } = refusal {
                        self.raise(
                            DecisionKind::FileScopeOverlap,
                            Some(a.task),
                            None,
                            format!("#{} は #{with} と担当ファイルが重なります", a.task),
                            "担当を分けるか順番に実行してください".into(),
                            vec!["reassign".into(), "reject".into()],
                        );
                    }
                }
            }
        }
    }

    /// タスクの指示文を作る。
    fn instruction_for(&self, task: &TeamTask, agent: &AgentId) -> String {
        let upstream: Vec<String> = task
            .dependencies
            .iter()
            .filter_map(|d| self.task(*d))
            .map(|d| {
                format!(
                    "#{} {}: {}",
                    d.id,
                    d.title,
                    if d.last_summary.is_empty() {
                        "(要約なし)"
                    } else {
                        d.last_summary.as_str()
                    }
                )
            })
            .collect();
        let forbidden: Vec<String> = self
            .tasks
            .iter()
            .filter(|t| t.id != task.id && t.state.is_held())
            .flat_map(|t| t.files.clone())
            .collect();
        let parent = self
            .agent(agent)
            .and_then(|a| a.parent_id.clone())
            .map(|p| p.0);
        let brief = super::prompt::Brief {
            goal: &self.goal,
            task,
            agent_id: agent.as_str(),
            parent_id: parent.as_deref(),
            workspace_root: "<ワークスペースルート>",
            upstream,
            forbidden_files: forbidden,
        };
        super::prompt::for_task(&brief, &self.tasks)
    }

    /// 停止待ちの Reassign を決着させる。
    ///
    /// **再起動をまたいでも進む**のが要点。`reassign_pending` はタスクに、
    /// 承認待ちは `decisions` に永続化されるので、復元後もここで続きから
    /// 進む — 旧プロセスは再起動で死んでいるので、担当が居なくなった時点で
    /// 安全に回収できる。
    fn settle_reassign(&mut self) {
        let waiting: Vec<TaskId> = self
            .tasks
            .iter()
            .filter(|t| t.reassign_pending)
            .map(|t| t.id)
            .collect();
        for id in waiting {
            if self.live_session_of(id).is_none() {
                // 担当セッションはもう居ない = 停止を確認できた。
                self.free_task(id, false);
                self.log(
                    TeamEventKind::TaskReady,
                    None,
                    None,
                    format!("#{id} の担当が停止したので配り直せます"),
                );
            }
        }
    }

    /// そのタスクの担当セッションのうち、**いま生きていると分かっている**もの。
    ///
    /// 「担当欄に値がある」だけでは足りない — セッションが消えていれば
    /// 止める相手は居ない。判断は `agents` の結び付き (最後の観測) で行う。
    fn live_session_of(&self, task: TaskId) -> Option<SessionId> {
        let sid = self.task(task)?.assigned_session?;
        self.agents
            .iter()
            .any(|a| a.session_id == Some(sid))
            .then_some(sid)
    }

    /// タスクの担当を解いて `Ready` へ戻す。
    ///
    /// `count_attempt` が真のときだけ試行回数を数える (セッションが落ちた
    /// 場合は数え、人が配り直した場合は数えない — 人の操作を失敗として
    /// 記録すると、上限に早く当たって使えなくなる)。
    fn free_task(&mut self, task: TaskId, count_attempt: bool) {
        let max = self.run.max_attempts;
        self.release_after_stop_confirmed(task);
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task) {
            if t.state.is_terminal() {
                t.reassign_pending = false;
                return;
            }
            if count_attempt {
                t.attempts = t.attempts.saturating_add(1);
            }
            t.assigned_agent = None;
            t.assigned_session = None;
            t.reassign_pending = false;
            t.validation.running = false;
            t.review.running = false;
            t.state = if t.attempts >= max {
                TeamTaskState::NeedsUser
            } else {
                // **人の操作 / 観測済みの消滅による解放。** 状態機械の表には
                // 「Running → Ready」が無い (自動処理には許さない) ので、
                // ここは意図的に `force` を通る。根拠は上の 2 つに限られ、
                // どちらも「もう誰も触っていない」ことが確認済み。
                sm::force(t.state, TeamTaskState::Ready)
            };
            t.updated_at = now_secs();
        }
        self.decisions.retain(|d| {
            !(d.kind == DecisionKind::StopAgents && d.task_id == Some(task))
        });
        self.dirty = true;
    }

    /// 停止が確認できたタスクについて、既存調停層へ引き渡してよいと伝える。
    fn release_after_stop_confirmed(&mut self, task: TaskId) {
        if let Some(ct) = self.task(task).and_then(|t| t.coordinator_task) {
            self.co.confirm_stopped(ct, Instant::now());
        }
    }

    /// **既存調停層に「前任者はもう触っていない」と伝える。**
    ///
    /// `coordinator` は前任者の停止が未確認のタスクを引き渡さない
    /// (同時編集で成果物が壊れるため)。ここで確認を通してよいのは
    /// **担当が自分から手を離したと分かっている場合だけ**:
    ///
    /// * 担当が完了 / 失敗を報告した — もう編集していない
    /// * 担当が出した成果に対する検証が失敗した — 同上
    ///
    /// **「人が Reassign を押した」は根拠にならない。** 押した時点では
    /// 旧担当はまだ動いていて、ファイルを編集しているかもしれない。その
    /// 経路は [`TeamAction::ReassignTask`] が停止承認 → `StopAgent` →
    /// セッション消滅の観測、という順で通す。
    ///
    /// セッションが消えた場合は [`release_dead`](Self::release_dead) が
    /// `note_exited` → `confirm_stopped` の順で通す。**順序は飛ばさない。**
    fn release_after_self_report(&mut self, task_id: TaskId) {
        if let Some(ct) = self.task(task_id).and_then(|t| t.coordinator_task) {
            self.co.confirm_stopped(ct, Instant::now());
        }
    }

    /// タスクを完了として締める。
    fn complete_task(&mut self, id: TaskId, agent: &AgentId) {
        let at = Instant::now();
        if let Some(ct) = self.task(id).and_then(|t| t.coordinator_task) {
            self.co.note_done(ct, at);
        }
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.assigned_session = None;
            t.updated_at = now_secs();
        }
        self.log(
            TeamEventKind::TaskCompleted,
            Some(agent.clone()),
            None,
            format!("#{id} を完了しました"),
        );
        self.dirty = true;
    }

    /// レビュータスクを作る。
    fn new_review_task(&mut self, target: &TeamTask) -> TeamTask {
        let id = self.next_task_id;
        self.next_task_id += 1;
        let now = now_secs();
        let qa = self
            .teams
            .iter()
            .find(|t| t.lead_role == TeamRole::Reviewer)
            .map(|t| t.id.clone())
            .unwrap_or_else(|| TeamId::new("qa"));
        TeamTask {
            id,
            goal_id: self.goal.id.clone(),
            key: format!("review-{}", target.key),
            title: format!("#{} のレビュー", target.id),
            description: format!("#{} 「{}」をレビューする", target.id, target.title),
            team_id: qa,
            role: TeamRole::Reviewer,
            dependencies: Vec::new(),
            // **レビューはコードを触らない**ので担当ファイルを持たない
            // (持つと実装タスクと重なって永久に配れない)。
            files: Vec::new(),
            required_caps: Vec::new(),
            acceptance_criteria: target.acceptance_criteria.clone(),
            validation_commands: Vec::new(),
            state: TeamTaskState::Ready,
            assigned_agent: None,
            assigned_session: None,
            attempts: 0,
            review_of: Some(target.id),
            coordinator_task: None,
            validation: ValidationState::default(),
            review: ReviewState::default(),
            context: Vec::new(),
            reported_validation: Vec::new(),
            reassign_pending: false,
            last_summary: String::new(),
            changed_files: Vec::new(),
            blockers: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Goal の状態を Task Graph から更新する。
    fn update_goal(&mut self) {
        if self.goal.status.is_terminal() {
            return;
        }
        let done = graph::goal_done(
            &self.tasks,
            &self.goal.definition_of_done,
            self.run.review_required,
        );
        let next = if done {
            GoalStatus::Completed
        } else if !self.decisions.is_empty() {
            GoalStatus::NeedsUser
        } else if self.run.paused {
            GoalStatus::Paused
        } else if self.goal.status == GoalStatus::Ready || self.goal.status == GoalStatus::Planning
        {
            self.goal.status
        } else if self
            .tasks
            .iter()
            .any(|t| t.state == TeamTaskState::Reviewing)
        {
            GoalStatus::Reviewing
        } else if self
            .tasks
            .iter()
            .any(|t| t.role == TeamRole::Integrator && t.state.is_held())
        {
            GoalStatus::Integrating
        } else if !self.tasks.is_empty()
            && self
                .tasks
                .iter()
                .all(|t| t.state == TeamTaskState::Blocked || t.state.is_terminal())
            && self.tasks.iter().any(|t| t.state == TeamTaskState::Blocked)
        {
            GoalStatus::Blocked
        } else {
            GoalStatus::Running
        };
        if next != self.goal.status {
            self.goal.status = next;
            self.goal.updated_at = now_secs();
            self.dirty = true;
            if next == GoalStatus::Completed {
                self.log(
                    TeamEventKind::GoalCompleted,
                    None,
                    None,
                    "Goal Completed — Definition of Done を満たしました".into(),
                );
            }
        }
    }

    // ── 補助 ──

    /// 起動するエージェントの顔ぶれを決める (計画時に 1 回)。
    fn plan_roster(&mut self) {
        let now = now_secs();
        let n = scheduler::desired_sessions(&self.tasks, self.run.agent_count).max(1);
        let lead_team = self
            .teams
            .first()
            .map(|t| t.id.clone())
            .unwrap_or_else(|| TeamId::new("implementation"));
        let lead = AgentId::new("team-lead");
        self.agents.push(TeamAgent {
            id: lead.clone(),
            name: "Team Lead".to_string(),
            role: TeamRole::TeamLead,
            team_id: lead_team,
            parent_id: None,
            kind: AgentKind::ManagedSession,
            session_id: None,
            provider: String::new(),
            state: AgentWorkState::Idle,
            current_task: None,
            current_action: String::new(),
            children: Vec::new(),
            created_at: now,
            last_activity_at: now,
        });
        for i in 1..n {
            let team = self
                .teams
                .get(i % self.teams.len().max(1))
                .map(|t| t.id.clone())
                .unwrap_or_else(|| TeamId::new("implementation"));
            let id = AgentId::new(format!("agent-{i}"));
            self.agents.push(TeamAgent {
                id: id.clone(),
                name: format!("Agent {i}"),
                role: TeamRole::Implementer,
                team_id: team,
                parent_id: Some(lead.clone()),
                kind: AgentKind::ManagedSession,
                session_id: None,
                provider: String::new(),
                state: AgentWorkState::Idle,
                current_task: None,
                current_action: String::new(),
                children: Vec::new(),
                created_at: now,
                last_activity_at: now,
            });
            if let Some(l) = self.agents.iter_mut().find(|a| a.id == lead) {
                l.children.push(id);
            }
        }
    }

    fn log(
        &mut self,
        kind: TeamEventKind,
        actor: Option<AgentId>,
        target: Option<AgentId>,
        summary: String,
    ) {
        let id = self.next_event_id;
        self.next_event_id += 1;
        self.events.push_back(TeamEvent {
            id,
            at: now_secs(),
            kind,
            actor,
            target,
            task_id: None,
            summary: clamp_text(&summary),
        });
        while self.events.len() > EVENT_CAP {
            self.events.pop_front();
        }
        self.dirty = true;
    }

    /// 判断を積む (同じ鍵のものは二重に積まない)。
    #[allow(clippy::too_many_arguments)]
    fn make_decision(
        &mut self,
        kind: DecisionKind,
        task_id: Option<TaskId>,
        agent_id: Option<AgentId>,
        reason: String,
        impact: String,
        options: Vec<String>,
        key: String,
    ) -> Option<Decision> {
        if self.decisions.iter().any(|d| d.idempotency_key == key) {
            return None;
        }
        let id = self.next_event_id;
        self.next_event_id += 1;
        let d = Decision {
            id,
            kind,
            at: now_secs(),
            task_id,
            agent_id,
            reason: clamp_text(&reason),
            impact: clamp_text(&impact),
            options,
            idempotency_key: key,
        };
        self.decisions.push(d.clone());
        self.decisions.sort_by(|a, b| {
            a.kind
                .priority()
                .cmp(&b.kind.priority())
                .then(a.id.cmp(&b.id))
        });
        self.log(
            TeamEventKind::DecisionRaised,
            None,
            None,
            format!("人の判断が必要です: {}", d.reason),
        );
        Some(d)
    }

    fn raise(
        &mut self,
        kind: DecisionKind,
        task_id: Option<TaskId>,
        agent_id: Option<AgentId>,
        reason: String,
        impact: String,
        options: Vec<String>,
    ) {
        let key = format!(
            "{}:{}",
            kind.key(),
            task_id.map(|t| t.to_string()).unwrap_or_default()
        );
        self.make_decision(kind, task_id, agent_id, reason, impact, options, key);
    }
}

/// 起動オプション。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunOptions {
    pub run_id: String,
    pub spec_source: String,
    pub agent_count: usize,
    pub max_attempts: u8,
    pub review_required: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            run_id: format!("run-{}", now_secs()),
            spec_source: String::new(),
            agent_count: 4,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            review_required: true,
        }
    }
}

/// 表示用の状態 → 調停層の状態 (割り当て可否の判断へ渡すため)。
fn work_to_session_state(w: AgentWorkState) -> SessionState {
    match w {
        AgentWorkState::Idle | AgentWorkState::Completed => SessionState::Idle,
        AgentWorkState::Exited => SessionState::Exited,
        AgentWorkState::WaitingApproval => SessionState::WaitingApproval,
        AgentWorkState::Stalled => SessionState::Stalled,
        AgentWorkState::Unknown => SessionState::Unknown,
        _ => SessionState::Working,
    }
}

/// 却下ログで「誰の担当か」を出すための小道具。
fn task_id_as_agent(t: &TeamTask) -> AgentId {
    t.assigned_agent
        .clone()
        .unwrap_or_else(|| AgentId::new("(未割り当て)"))
}
