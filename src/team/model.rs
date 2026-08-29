//! Team 機能の**核となる型**。egui にも既存 UI にも依存しない純データ層。
//!
//! ## なぜ独立した型を持つのか
//!
//! [`crate::coordinator::Task`] は「誰に配ってよいか」の規則を持つ調停層の型で、
//! 依存関係・受入基準・レビュー・検証コマンドといった**チーム開発の語彙**を
//! 持たない。ここへ後から足すと、既存の割り当て規則 (fail-closed の重なり
//! 判定) を触ることになり、いま守れている性質を壊しかねない。
//!
//! そこで Team 側は**自分のメタデータを持ち**、実際に配るときだけ
//! `coordinator` へ橋渡しする ([`super::runtime`])。既存の安全制御は
//! 迂回せず、その上に載る。
//!
//! ## 時刻は Unix 秒だけ
//!
//! `Instant` は**永続化できない** (プロセスを跨ぐと意味を失う)。Team は
//! 再起動をまたいで状態を復元するので、記録は全部 `u64` の Unix 秒にする。

use std::fmt;

use serde::{Deserialize, Serialize};

/// 既存の調停層と同じセッション ID (`terminal::Session::id`)。
pub type SessionId = crate::coordinator::SessionId;

/// Team のタスク ID。**`coordinator::TaskId` とは別空間**なので、
/// 対応は [`TeamTask::coordinator_task`] で明示的に持つ。
pub type TaskId = u64;

/// イベント ID (単調増加)。
pub type EventId = u64;

/// 1 文字も切り詰めずに保存してよい説明文の上限。
///
/// 上限が無いと、暴走したエージェントの出力がそのまま永続ファイルへ入り、
/// 次の起動で読めなくなる。
pub const TEXT_MAX: usize = 4_000;

/// 配列の要素数上限 (受入基準・検証コマンド・ファイル一覧など)。
pub const LIST_MAX: usize = 64;

/// 現在時刻 (Unix 秒)。システム時計が壊れていても 0 に落ちるだけで panic しない。
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 文字列を上限で切る。**切ったことが分かるように印を付ける**
/// (黙って消すと「報告したのに読まれていない」と区別が付かない)。
pub fn clamp_text(s: &str) -> String {
    if s.len() <= TEXT_MAX {
        return s.to_string();
    }
    let mut cut = TEXT_MAX;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…(切り詰め)", &s[..cut])
}

/// 一覧を上限で切る。
pub fn clamp_list(v: Vec<String>) -> Vec<String> {
    v.into_iter()
        .take(LIST_MAX)
        .map(|s| clamp_text(&s))
        .collect()
}

// ── ID の新型 ────────────────────────────────────────────────────────

macro_rules! string_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }
    };
}

string_id!(
    /// Goal の識別子。1 つの Team Run に 1 つ。
    GoalId
);
string_id!(
    /// 専門チームの識別子 (`"backend"` など、Planner が付ける `key`)。
    TeamId
);
string_id!(
    /// エージェントの識別子 (`"backend-api-1"` など)。
    ///
    /// **Zaivern が起動したセッションにも、親が報告してきた内部サブ
    /// エージェントにも同じ空間を使う。** 実在するかどうかは
    /// [`AgentKind`] が区別する — ここを混ぜると「開けない端末のボタン」
    /// が生える。
    AgentId
);

// ── Goal ─────────────────────────────────────────────────────────────

/// Goal の進み具合。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// 計画中 (Planner が動いている / 計画を人が見ている)。
    Planning,
    /// 計画が確定し、開始待ち。
    Ready,
    /// 実行中。
    Running,
    /// 人が止めている。実行中エージェントの監視は続く。
    Paused,
    /// 進められない (依存が全部詰まっている)。
    Blocked,
    /// レビュー待ちが主。
    Reviewing,
    /// 最終統合中。
    Integrating,
    /// Definition of Done を全部満たした。
    Completed,
    /// 失敗で終わった。
    Failed,
    /// 人の判断が要る。
    NeedsUser,
}

impl GoalStatus {
    /// 表示用の安定 ID (i18n の鍵にもなる)。
    pub fn key(self) -> &'static str {
        match self {
            GoalStatus::Planning => "planning",
            GoalStatus::Ready => "ready",
            GoalStatus::Running => "running",
            GoalStatus::Paused => "paused",
            GoalStatus::Blocked => "blocked",
            GoalStatus::Reviewing => "reviewing",
            GoalStatus::Integrating => "integrating",
            GoalStatus::Completed => "completed",
            GoalStatus::Failed => "failed",
            GoalStatus::NeedsUser => "needs_user",
        }
    }

    /// もう動かさない状態か。
    pub fn is_terminal(self) -> bool {
        matches!(self, GoalStatus::Completed | GoalStatus::Failed)
    }
}

/// 開発の到達目標。**Definition of Done を必ず持つ。**
///
/// 「エージェントが完了と言った」だけで [`GoalStatus::Completed`] にしない
/// のがこの型の存在理由で、判定は [`super::graph::goal_done`] が
/// Task Graph と検証結果から機械的に出す。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamGoal {
    pub id: GoalId,
    pub title: String,
    /// SPEC の本文 (切り詰め済み)。
    pub specification: String,
    /// 完了条件。**空を許さない** ([`super::graph::validate_plan`] が弾く)。
    pub definition_of_done: Vec<String>,
    pub status: GoalStatus,
    pub created_at: u64,
    pub updated_at: u64,
}

impl TeamGoal {
    pub fn new(id: GoalId, title: impl Into<String>, spec: &str, dod: Vec<String>) -> Self {
        let now = now_secs();
        Self {
            id,
            title: clamp_text(&title.into()),
            specification: clamp_text(spec),
            definition_of_done: clamp_list(dod),
            status: GoalStatus::Planning,
            created_at: now,
            updated_at: now,
        }
    }
}

// ── 専門チーム ───────────────────────────────────────────────────────

/// 専門チーム 1 つ (Organization Board のレーン 1 本)。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamGroup {
    pub id: TeamId,
    pub name: String,
    /// 親エージェントの役割 (レーンの先頭カードになる)。
    pub lead_role: TeamRole,
}

// ── 役割 ─────────────────────────────────────────────────────────────

/// チーム内の役割。
///
/// **実装担当とレビュー担当を同じセッションにしない**という規則
/// ([`super::scheduler`]) がここを見る。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamRole {
    TeamLead,
    Planner,
    Architect,
    Implementer,
    Tester,
    Reviewer,
    Integrator,
}

impl TeamRole {
    /// 安定 ID。永続ファイルにも i18n の鍵にもこの綴りが載る。
    pub fn key(self) -> &'static str {
        match self {
            TeamRole::TeamLead => "team_lead",
            TeamRole::Planner => "planner",
            TeamRole::Architect => "architect",
            TeamRole::Implementer => "implementer",
            TeamRole::Tester => "tester",
            TeamRole::Reviewer => "reviewer",
            TeamRole::Integrator => "integrator",
        }
    }

    /// 文字列から復元する。未知の綴りは `Implementer` に倒す
    /// (Planner の出力を信用しすぎない — 未知の役割で全部止めない)。
    pub fn parse(s: &str) -> TeamRole {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "team_lead" | "lead" | "backend_lead" | "frontend_lead" | "qa_lead" => {
                TeamRole::TeamLead
            }
            "planner" => TeamRole::Planner,
            "architect" => TeamRole::Architect,
            "tester" | "qa" => TeamRole::Tester,
            "reviewer" | "review" => TeamRole::Reviewer,
            "integrator" | "integration" => TeamRole::Integrator,
            _ => TeamRole::Implementer,
        }
    }

    /// 一覧 (UI のプリセットとテストの網羅で使う)。
    pub const ALL: [TeamRole; 7] = [
        TeamRole::TeamLead,
        TeamRole::Planner,
        TeamRole::Architect,
        TeamRole::Implementer,
        TeamRole::Tester,
        TeamRole::Reviewer,
        TeamRole::Integrator,
    ];
}

// ── エージェント ─────────────────────────────────────────────────────

/// エージェントの実体がどこにあるか。
///
/// **ここを混ぜてはいけない。** Zaivern が直接起動していない内部サブ
/// エージェントを実在するセッションとして描くと、「端末を開く」ボタンが
/// 押せるのに何も起きない — 画面が嘘をつく。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    /// Zaivern が起動し、`SessionId` を持ち、端末を開ける。
    ManagedSession,
    /// 親エージェントが構造化イベントで報告してきただけ。端末は無い。
    ReportedSubAgent,
}

/// エージェントの作業状態。**UI 専用の第 2 の真実にしない** —
/// [`super::roles::derive_agent_work_state`] が既存の
/// [`crate::coordinator::SessionState`] とタスク・検証・レビューから導出する。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentWorkState {
    Idle,
    Planning,
    Coordinating,
    Working,
    Testing,
    Reviewing,
    WaitingApproval,
    Blocked,
    Stalled,
    Completed,
    Exited,
    Unknown,
}

impl AgentWorkState {
    pub fn key(self) -> &'static str {
        match self {
            AgentWorkState::Idle => "idle",
            AgentWorkState::Planning => "planning",
            AgentWorkState::Coordinating => "coordinating",
            AgentWorkState::Working => "working",
            AgentWorkState::Testing => "testing",
            AgentWorkState::Reviewing => "reviewing",
            AgentWorkState::WaitingApproval => "waiting_approval",
            AgentWorkState::Blocked => "blocked",
            AgentWorkState::Stalled => "stalled",
            AgentWorkState::Completed => "completed",
            AgentWorkState::Exited => "exited",
            AgentWorkState::Unknown => "unknown",
        }
    }

    /// **色だけに頼らない**ための記号。色覚特性や単色端末でも読める。
    pub fn glyph(self) -> &'static str {
        match self {
            AgentWorkState::Idle => "○",
            AgentWorkState::Planning => "◈",
            AgentWorkState::Coordinating => "◈",
            AgentWorkState::Working => "●",
            AgentWorkState::Testing => "◆",
            AgentWorkState::Reviewing => "◎",
            AgentWorkState::WaitingApproval => "!",
            AgentWorkState::Blocked => "▲",
            AgentWorkState::Stalled => "⚠",
            AgentWorkState::Completed => "✓",
            AgentWorkState::Exited => "×",
            AgentWorkState::Unknown => "?",
        }
    }

    /// 点滅させてよいか。**停滞と緊急承認だけ** (設計原則 3: アイドルの
    /// コストはゼロ。常時アニメーションはバッテリーのバグ)。
    pub fn may_blink(self) -> bool {
        matches!(
            self,
            AgentWorkState::Stalled | AgentWorkState::WaitingApproval
        )
    }
}

/// チームの一員。親子関係を**型として**持つ。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamAgent {
    pub id: AgentId,
    pub name: String,
    pub role: TeamRole,
    pub team_id: TeamId,
    /// 親エージェント。[`AgentKind::ReportedSubAgent`] は**必須**
    /// ([`super::graph::validate_agents`] が強制する)。
    pub parent_id: Option<AgentId>,
    pub kind: AgentKind,
    /// 実セッション。`ManagedSession` でも起動前は `None`。
    pub session_id: Option<SessionId>,
    /// 使っている CLI (`claude` / `codex` など)。表示だけに使う。
    pub provider: String,
    pub state: AgentWorkState,
    pub current_task: Option<TaskId>,
    pub current_action: String,
    pub children: Vec<AgentId>,
    pub created_at: u64,
    pub last_activity_at: u64,
}

impl TeamAgent {
    /// 端末を開けるか。UI のボタンの有効・無効はここだけを見る。
    pub fn can_open_terminal(&self) -> bool {
        self.kind == AgentKind::ManagedSession && self.session_id.is_some()
    }
}

// ── 検証とレビュー ───────────────────────────────────────────────────

/// 検証コマンド 1 本の結果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationRun {
    pub command: String,
    pub exit_code: i32,
}

/// タスクの検証状態。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationState {
    /// 実行中か (UI は Testing を出す)。
    pub running: bool,
    /// 走らせた結果。**空 = 未実行**で、これだけで Completed にはしない。
    pub runs: Vec<ValidationRun>,
}

impl ValidationState {
    /// 全部走って全部成功したか。要求されたコマンドの一覧を渡す。
    pub fn passed(&self, required: &[String]) -> bool {
        if self.running {
            return false;
        }
        if required.is_empty() {
            // **検証コマンドが 1 本も無い計画は validate_plan が弾く**ので、
            // ここへ来るのは復元した壊れかけの状態だけ。安全側 (未検証) に倒す。
            return false;
        }
        required.iter().all(|c| {
            self.runs
                .iter()
                .any(|r| r.command == *c && r.exit_code == 0)
        })
    }

    /// 1 本でも失敗しているか。
    pub fn failed(&self) -> bool {
        self.runs.iter().any(|r| r.exit_code != 0)
    }
}

/// レビューの判定。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
}

/// タスクのレビュー状態。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewState {
    /// レビュー中か。
    pub running: bool,
    /// 担当したレビュアー (実装担当と**別セッション**であること)。
    pub reviewer: Option<AgentId>,
    pub reviewer_session: Option<SessionId>,
    pub verdict: Option<ReviewVerdict>,
    /// 指摘。次の指示へそのまま載る。
    pub findings: Vec<String>,
}

impl ReviewState {
    pub fn approved(&self) -> bool {
        !self.running && self.verdict == Some(ReviewVerdict::Approve)
    }
}

// ── タスク ───────────────────────────────────────────────────────────

/// Team のタスク状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamTaskState {
    /// 依存が未完了。
    Pending,
    /// 依存が全部完了し、配れる。
    Ready,
    /// 担当が決まった (まだ動き出していない)。
    Assigned,
    /// 実装中。
    Running,
    /// 進められない。
    Blocked,
    /// 検証コマンドを走らせている。
    Validating,
    /// レビュー中。
    Reviewing,
    /// 指摘が出たので直す。
    RevisionRequired,
    /// 失敗。
    Failed,
    /// 完了 (**レビュー承認済み**)。
    Completed,
    /// 人の判断待ち。
    NeedsUser,
}

impl TeamTaskState {
    pub fn key(self) -> &'static str {
        match self {
            TeamTaskState::Pending => "pending",
            TeamTaskState::Ready => "ready",
            TeamTaskState::Assigned => "assigned",
            TeamTaskState::Running => "running",
            TeamTaskState::Blocked => "blocked",
            TeamTaskState::Validating => "validating",
            TeamTaskState::Reviewing => "reviewing",
            TeamTaskState::RevisionRequired => "revision_required",
            TeamTaskState::Failed => "failed",
            TeamTaskState::Completed => "completed",
            TeamTaskState::NeedsUser => "needs_user",
        }
    }

    /// これ以上動かさない状態か。
    pub fn is_terminal(self) -> bool {
        matches!(self, TeamTaskState::Completed | TeamTaskState::NeedsUser)
    }

    /// **その担当が今まさに手を動かしている**状態か。
    ///
    /// [`is_held`](Self::is_held) との違いが要るのは、レビュー待ちの
    /// タスクは「ファイルは押さえたまま」だが「実装担当の手は空いている」
    /// から。ここを一緒にすると、レビュー待ちが 1 本あるだけで実装担当が
    /// 永久に忙しい扱いになり、**レビュー担当の候補が枯れて誰も進めない**
    /// (2 体 2 タスクで実際に詰まった)。
    pub fn is_working(self) -> bool {
        matches!(
            self,
            TeamTaskState::Assigned | TeamTaskState::Running | TeamTaskState::Validating
        )
    }

    /// エージェントが握っている状態か (割り当て済み)。
    pub fn is_held(self) -> bool {
        matches!(
            self,
            TeamTaskState::Assigned
                | TeamTaskState::Running
                | TeamTaskState::Validating
                | TeamTaskState::Reviewing
        )
    }

    /// 一覧 (状態遷移テストの網羅で使う)。
    pub const ALL: [TeamTaskState; 11] = [
        TeamTaskState::Pending,
        TeamTaskState::Ready,
        TeamTaskState::Assigned,
        TeamTaskState::Running,
        TeamTaskState::Blocked,
        TeamTaskState::Validating,
        TeamTaskState::Reviewing,
        TeamTaskState::RevisionRequired,
        TeamTaskState::Failed,
        TeamTaskState::Completed,
        TeamTaskState::NeedsUser,
    ];
}

/// 開発タスク 1 件。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTask {
    pub id: TaskId,
    pub goal_id: GoalId,
    /// Planner が付けた安定キー (`"auth-api"`)。依存の解決に使う。
    pub key: String,
    pub title: String,
    pub description: String,
    pub team_id: TeamId,
    pub role: TeamRole,
    pub dependencies: Vec<TaskId>,
    /// 触るファイル (ワークスペース相対 / `/` 区切り / glob 可)。
    /// **既存の `coordinator::admit` へそのまま渡す**ので、重なりは
    /// 配る瞬間に fail-closed で止まる。
    pub files: Vec<String>,
    pub required_caps: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub validation_commands: Vec<String>,
    pub state: TeamTaskState,
    pub assigned_agent: Option<AgentId>,
    pub assigned_session: Option<SessionId>,
    /// 実装のやり直し回数。上限に達したら [`TeamTaskState::NeedsUser`]。
    pub attempts: u8,
    /// レビュータスクなら、レビュー対象の実装タスク。
    pub review_of: Option<TaskId>,
    /// 既存調停層のタスク ID (配ったときに埋まる)。
    pub coordinator_task: Option<crate::coordinator::TaskId>,
    pub validation: ValidationState,
    pub review: ReviewState,
    /// 次の担当へ渡す追加文脈 (レビュー指摘など)。
    pub context: Vec<String>,
    /// 直近の完了報告の要約。
    pub last_summary: String,
    /// 直近報告で変更されたファイル。担当外を触っていないかの照合に使う。
    pub changed_files: Vec<String>,
    pub blockers: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

// ── イベント ─────────────────────────────────────────────────────────

/// Activity Feed に出る出来事の種別。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamEventKind {
    GoalCreated,
    PlanReady,
    RunStarted,
    RunPaused,
    RunResumed,
    RunStopped,
    TaskReady,
    TaskAssigned,
    TaskStarted,
    ValidationStarted,
    ValidationCompleted,
    ReviewStarted,
    ReviewCompleted,
    RevisionRequested,
    TaskCompleted,
    TaskFailed,
    TaskBlocked,
    AgentStarted,
    AgentProgress,
    AgentBlocked,
    AgentCompleted,
    AgentFailed,
    SubAgentReported,
    DecisionRaised,
    DecisionResolved,
    Rejected,
    GoalCompleted,
}

impl TeamEventKind {
    pub fn key(self) -> &'static str {
        match self {
            TeamEventKind::GoalCreated => "goal_created",
            TeamEventKind::PlanReady => "plan_ready",
            TeamEventKind::RunStarted => "run_started",
            TeamEventKind::RunPaused => "run_paused",
            TeamEventKind::RunResumed => "run_resumed",
            TeamEventKind::RunStopped => "run_stopped",
            TeamEventKind::TaskReady => "task_ready",
            TeamEventKind::TaskAssigned => "task_assigned",
            TeamEventKind::TaskStarted => "task_started",
            TeamEventKind::ValidationStarted => "validation_started",
            TeamEventKind::ValidationCompleted => "validation_completed",
            TeamEventKind::ReviewStarted => "review_started",
            TeamEventKind::ReviewCompleted => "review_completed",
            TeamEventKind::RevisionRequested => "revision_requested",
            TeamEventKind::TaskCompleted => "task_completed",
            TeamEventKind::TaskFailed => "task_failed",
            TeamEventKind::TaskBlocked => "task_blocked",
            TeamEventKind::AgentStarted => "agent_started",
            TeamEventKind::AgentProgress => "agent_progress",
            TeamEventKind::AgentBlocked => "agent_blocked",
            TeamEventKind::AgentCompleted => "agent_completed",
            TeamEventKind::AgentFailed => "agent_failed",
            TeamEventKind::SubAgentReported => "sub_agent_reported",
            TeamEventKind::DecisionRaised => "decision_raised",
            TeamEventKind::DecisionResolved => "decision_resolved",
            TeamEventKind::Rejected => "rejected",
            TeamEventKind::GoalCompleted => "goal_completed",
        }
    }
}

/// 出来事 1 件。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamEvent {
    pub id: EventId,
    pub at: u64,
    pub kind: TeamEventKind,
    pub actor: Option<AgentId>,
    pub target: Option<AgentId>,
    pub task_id: Option<TaskId>,
    pub summary: String,
}

// ── 人間の判断 ───────────────────────────────────────────────────────

/// 人へ上げる理由。**MVP で自動実行しないもの**はここへ集まる。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// 仕様が矛盾している。
    SpecConflict,
    /// 担当ファイルが重なった (既存 `coordinator` が断った)。
    FileScopeOverlap,
    /// 危険なコマンドを要求された。
    DangerousCommand,
    /// 再試行の上限に達した。
    AttemptsExhausted,
    /// 割り当て候補がいない。
    NoCandidate,
    /// push / merge / deploy を求められた。**MVP では自動実行しない。**
    ReleaseOperation,
    /// コスト上限に達した。
    CostLimit,
    /// 破壊的変更。
    DestructiveChange,
    /// 権限昇格。
    PrivilegeEscalation,
    /// 実行中エージェントの停止 (既存 approval gate を通す)。
    StopAgents,
}

impl DecisionKind {
    pub fn key(self) -> &'static str {
        match self {
            DecisionKind::SpecConflict => "spec_conflict",
            DecisionKind::FileScopeOverlap => "file_scope_overlap",
            DecisionKind::DangerousCommand => "dangerous_command",
            DecisionKind::AttemptsExhausted => "attempts_exhausted",
            DecisionKind::NoCandidate => "no_candidate",
            DecisionKind::ReleaseOperation => "release_operation",
            DecisionKind::CostLimit => "cost_limit",
            DecisionKind::DestructiveChange => "destructive_change",
            DecisionKind::PrivilegeEscalation => "privilege_escalation",
            DecisionKind::StopAgents => "stop_agents",
        }
    }

    /// 優先度 (小さいほど先に出す)。Current Action Bar の順序もこれで決まる。
    pub fn priority(self) -> u8 {
        match self {
            DecisionKind::PrivilegeEscalation | DecisionKind::DestructiveChange => 0,
            DecisionKind::DangerousCommand | DecisionKind::ReleaseOperation => 1,
            DecisionKind::CostLimit | DecisionKind::StopAgents => 2,
            DecisionKind::SpecConflict | DecisionKind::FileScopeOverlap => 3,
            DecisionKind::AttemptsExhausted | DecisionKind::NoCandidate => 4,
        }
    }
}

/// 人に選んでもらう 1 件。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub id: EventId,
    pub kind: DecisionKind,
    pub at: u64,
    pub task_id: Option<TaskId>,
    pub agent_id: Option<AgentId>,
    /// 何が起きたか。
    pub reason: String,
    /// 影響範囲。
    pub impact: String,
    /// 選べる操作の安定 ID (`approve` / `reject` / `retry` / `reassign` …)。
    pub options: Vec<String>,
    /// 同じ判断を二重に積まないための鍵。
    pub idempotency_key: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 切り詰めは文字境界を壊さない() {
        let s = "あ".repeat(TEXT_MAX);
        let out = clamp_text(&s);
        assert!(out.ends_with("…(切り詰め)"));
        // UTF-8 として妥当なまま (String なので構築できた時点で保証されるが、
        // 境界を割ると panic するので「作れた」こと自体が検査になる)。
        assert!(out.len() <= TEXT_MAX + 32);
    }

    #[test]
    fn 短い文字列はそのまま() {
        assert_eq!(clamp_text("hello"), "hello");
    }

    #[test]
    fn 一覧は上限で切る() {
        let v: Vec<String> = (0..LIST_MAX + 10).map(|i| i.to_string()).collect();
        assert_eq!(clamp_list(v).len(), LIST_MAX);
    }

    #[test]
    fn 役割の綴りは往復する() {
        for r in TeamRole::ALL {
            assert_eq!(TeamRole::parse(r.key()), r, "{}", r.key());
        }
        // 未知は実装担当へ倒す (未知の役割で計画全体を落とさない)
        assert_eq!(TeamRole::parse("wizard"), TeamRole::Implementer);
    }

    #[test]
    fn 検証は要求された全コマンドの成功を要る() {
        let req = vec!["cargo test a".to_string(), "cargo test b".to_string()];
        let mut v = ValidationState::default();
        assert!(!v.passed(&req), "未実行で通してはいけない");
        v.runs.push(ValidationRun {
            command: "cargo test a".into(),
            exit_code: 0,
        });
        assert!(!v.passed(&req), "一部だけで通してはいけない");
        v.runs.push(ValidationRun {
            command: "cargo test b".into(),
            exit_code: 1,
        });
        assert!(!v.passed(&req), "失敗があるのに通してはいけない");
        assert!(v.failed());
    }

    #[test]
    fn 検証コマンドが空なら未検証扱い() {
        let v = ValidationState::default();
        assert!(!v.passed(&[]));
    }

    #[test]
    fn 実行中は通さない() {
        let req = vec!["x".to_string()];
        let v = ValidationState {
            running: true,
            runs: vec![ValidationRun {
                command: "x".into(),
                exit_code: 0,
            }],
        };
        assert!(!v.passed(&req));
    }

    #[test]
    fn 報告されたサブエージェントは端末を開けない() {
        let a = TeamAgent {
            id: AgentId::new("sub"),
            name: "sub".into(),
            role: TeamRole::Tester,
            team_id: TeamId::new("qa"),
            parent_id: Some(AgentId::new("lead")),
            kind: AgentKind::ReportedSubAgent,
            // 万一 session_id が入っていても開けない (kind が真実)
            session_id: Some(7),
            provider: "claude".into(),
            state: AgentWorkState::Working,
            current_task: None,
            current_action: String::new(),
            children: Vec::new(),
            created_at: 0,
            last_activity_at: 0,
        };
        assert!(!a.can_open_terminal());
        let managed = TeamAgent {
            kind: AgentKind::ManagedSession,
            ..a.clone()
        };
        assert!(managed.can_open_terminal());
        let not_started = TeamAgent {
            kind: AgentKind::ManagedSession,
            session_id: None,
            ..a
        };
        assert!(!not_started.can_open_terminal());
    }

    #[test]
    fn 点滅は停滞と承認だけ() {
        for s in [
            AgentWorkState::Idle,
            AgentWorkState::Working,
            AgentWorkState::Testing,
            AgentWorkState::Reviewing,
            AgentWorkState::Completed,
        ] {
            assert!(!s.may_blink(), "{}", s.key());
        }
        assert!(AgentWorkState::Stalled.may_blink());
        assert!(AgentWorkState::WaitingApproval.may_blink());
    }

    #[test]
    fn 状態の記号は全部違う() {
        let mut seen = std::collections::BTreeSet::new();
        for s in [
            AgentWorkState::Idle,
            AgentWorkState::Working,
            AgentWorkState::Testing,
            AgentWorkState::Reviewing,
            AgentWorkState::WaitingApproval,
            AgentWorkState::Blocked,
            AgentWorkState::Stalled,
            AgentWorkState::Completed,
            AgentWorkState::Exited,
        ] {
            assert!(seen.insert(s.glyph()), "記号が重複: {}", s.key());
        }
    }
}
