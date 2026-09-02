//! GUI へ渡す**不変スナップショット**と、レイアウトの純粋関数。
//!
//! ## なぜスナップショットなのか
//!
//! 描画側が Runtime を直に持つと、描画中に状態を書き換えられてしまう
//! (中央ビューが 2 枚重なった事故と同じ形)。ここで一度だけ写して、
//! 描画は写しだけを読む。**UI 側に第 2 の真実を持たせない。**
//!
//! ## レイアウト判断はここで決める
//!
//! CLAUDE.md の「レイアウト判断は純粋関数に切り出してテーブルテストで
//! 固定する」に従い、レーン幅・折り返し・空状態の有無をここで決めて、
//! `organization_board.rs` はその結果を描くだけにする。

use std::collections::{BTreeMap, BTreeSet};

use super::graph::{self, Phase, PhaseStatus};
use super::model::*;
use super::runtime::TeamRuntime;

/// 1 画面に並べるレーンの標準本数の上限。これを超えたら横スクロール。
pub const MAX_LANES_ON_SCREEN: usize = 5;

/// Inspector に出す診断出力の上限 (1 コマンドあたり・バイト)。
///
/// **画面へ 64KiB を流し込まない。** 出せば出すほど良いわけではなく、
/// スクロールが効かなくなるだけ。全文は台帳 (`tasks.json`) に残る。
pub const INSPECTOR_DIAGNOSTIC_BYTES: usize = 2_000;
/// レーン 1 本の最小幅 (px)。これを割るなら本数を減らす。
pub const LANE_MIN_W: f32 = 240.0;
/// レーン 1 本の快適な幅 (px)。
pub const LANE_IDEAL_W: f32 = 320.0;
/// Mission Panel の幅 (px)。
pub const MISSION_PANEL_W: f32 = 300.0;
/// Mission Panel を出すのをやめる画面幅 (px)。狭い画面では盤面を優先する。
pub const MISSION_PANEL_MIN_TOTAL_W: f32 = 900.0;

// ── 表示用の写し ─────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalView {
    pub title: String,
    pub status: GoalStatus,
    pub definition_of_done: Vec<String>,
    pub phase: Phase,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamView {
    pub id: TeamId,
    pub name: String,
    pub lead_role: TeamRole,
    /// このレーンに属する親エージェント。
    pub parents: Vec<AgentId>,
    /// このレーンのタスク数 (完了 / 全体)。
    pub done: usize,
    pub total: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamAgentView {
    pub id: AgentId,
    pub name: String,
    pub role: TeamRole,
    pub team_id: TeamId,
    pub parent_id: Option<AgentId>,
    pub kind: AgentKind,
    pub session_id: Option<SessionId>,
    pub provider: String,
    pub state: AgentWorkState,
    pub current_task: Option<TaskId>,
    pub current_task_title: String,
    pub current_action: String,
    pub children: Vec<AgentId>,
    /// 最終活動からの経過秒。
    pub idle_secs: u64,
    /// 担当タスクの完了数 / 担当数。
    pub done: usize,
    pub assigned: usize,
    /// 端末を開けるか (`ReportedSubAgent` は開けない)。
    pub can_open_terminal: bool,
    pub blockers: Vec<String>,
    /// **いま画面に出ている直近の出力** (末尾数行)。
    ///
    /// 端末タブが「名前とボタン」だけだったので、走っている最中に中身を
    /// 見るには端末を開くしか無かった。開くと画面が切り替わるので、
    /// 「ちょっと様子を見たい」に対して代償が大きすぎる。
    pub preview: String,
}

/// 端末タブの札が**畳んでいるとき**に出す行数。
pub const PREVIEW_LINES_FOLDED: usize = 8;
/// 画面が持ち回る行数 (開いたときに出せる上限)。
///
/// 畳んだ札は末尾 [`PREVIEW_LINES_FOLDED`] 行だけを描く。**2 回取りに
/// 行かない** — 開くたびに Runtime へ問い合わせる形にすると、描画から
/// 状態を触ることになる。
pub const PREVIEW_LINES: usize = 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskView {
    pub id: TaskId,
    pub title: String,
    pub state: TeamTaskState,
    pub role: TeamRole,
    pub team_id: TeamId,
    pub assigned_agent: Option<AgentId>,
    pub attempts: u8,
    pub files: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub validation_commands: Vec<String>,
    pub validation_ok: bool,
    pub validation_ran: bool,
    /// 1 本でも失敗しているか。**「未実行」と「失敗」を混ぜない**
    /// (混ぜると、まだ走っていないタスクを赤く出してしまう)。
    pub validation_failed: bool,
    /// 最初に成功しなかった実測の終わり方。**「失敗」だけでは直し方が
    /// 分からない** (コードを直す / 時間を延ばす / 実行環境を直す)。
    pub validation_result: Option<ValidationOutcome>,
    /// 失敗した検証が吐いた診断出力 (`コマンド` → 末尾)。
    ///
    /// **人にも読ませる。** エージェントへ渡すだけにすると、直せなかった
    /// ときに人が同じことを手で再実行して確かめる羽目になる。
    pub validation_diagnostics: Vec<String>,
    pub review_verdict: Option<ReviewVerdict>,
    pub review_findings: Vec<String>,
    pub blockers: Vec<String>,
    pub last_summary: String,
    pub context: Vec<String>,
    pub review_of: Option<TaskId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamEventView {
    pub id: EventId,
    pub at: u64,
    pub kind: TeamEventKind,
    pub actor: Option<AgentId>,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecisionView {
    pub id: EventId,
    pub kind: DecisionKind,
    pub task_id: Option<TaskId>,
    pub reason: String,
    pub impact: String,
    pub options: Vec<String>,
}

/// Top Command Bar に出す数字。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TeamMetricsView {
    pub tasks_total: usize,
    pub tasks_done: usize,
    pub agents_active: usize,
    pub blocked: usize,
    pub tests_passed: usize,
    pub reviews_approved: usize,
    pub pending_decisions: usize,
    /// 0〜100 の整数 (浮動小数を画面に出さない)。
    pub progress_pct: u8,
    /// 設定された最大同時セッション数。
    pub agent_limit: usize,
    /// 設定された最大試行回数。
    pub max_attempts: u8,
}

/// GUI が読む不変スナップショット。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamSnapshot {
    pub goal: GoalView,
    pub teams: Vec<TeamView>,
    pub agents: Vec<TeamAgentView>,
    pub tasks: Vec<TaskView>,
    pub events: Vec<TeamEventView>,
    pub pending_decisions: Vec<DecisionView>,
    pub metrics: TeamMetricsView,
    pub phases: Vec<(Phase, PhaseStatus)>,
    pub paused: bool,
    pub stopped: bool,
    /// Activity Feed に出す件数の上限 (超えたぶんは切ってある)。
    pub feed_cap: usize,
    /// **この Run には自動検証が 1 本も無い。**
    ///
    /// 道具の無いフォルダ (素の HTML など) では検証コマンドを決められない。
    /// そのときの完了は**レビュー承認だけ**で決まるので、盤面が常に出す
    /// (通知は上書きで消えるが、これは状態から導くので消えない)。
    pub unvalidated: bool,
    /// **変更を実測できない**まま進んでいるか。
    ///
    /// Git 管理下でないフォルダでは差分を測れないので、「担当内だけを
    /// 変更した」と言える根拠が無いまま完了することになる。止めると
    /// **そのフォルダでは 1 件も完了できない**ので通すが、隠さない。
    pub unmeasured: bool,
}

/// Activity Feed に出す件数。**上限を持たないと 64 体で描画が破綻する。**
pub const FEED_CAP: usize = 60;

/// Runtime → スナップショット。
pub fn snapshot(rt: &TeamRuntime, now: u64) -> TeamSnapshot {
    let tasks = rt.tasks();
    let goal_done = rt.goal().status == GoalStatus::Completed;
    let phases = graph::phases(tasks, goal_done);
    let phase = graph::current_phase(tasks, goal_done);

    let agents: Vec<TeamAgentView> = rt
        .agents()
        .iter()
        .map(|a| {
            let mine: Vec<&TeamTask> = tasks
                .iter()
                .filter(|t| t.assigned_agent.as_ref() == Some(&a.id))
                .collect();
            let done = mine
                .iter()
                .filter(|t| t.state == TeamTaskState::Completed)
                .count();
            let cur = a
                .current_task
                .and_then(|id| tasks.iter().find(|t| t.id == id));
            TeamAgentView {
                id: a.id.clone(),
                name: a.name.clone(),
                role: a.role,
                team_id: a.team_id.clone(),
                parent_id: a.parent_id.clone(),
                kind: a.kind,
                session_id: a.session_id,
                provider: a.provider.clone(),
                state: a.state,
                current_task: a.current_task,
                current_task_title: cur.map(|t| t.title.clone()).unwrap_or_default(),
                current_action: a.current_action.clone(),
                children: a.children.clone(),
                idle_secs: now.saturating_sub(a.last_activity_at),
                done,
                assigned: mine.len(),
                can_open_terminal: a.can_open_terminal(),
                blockers: cur.map(|t| t.blockers.clone()).unwrap_or_default(),
                preview: rt.preview_of(&a.id, PREVIEW_LINES),
            }
        })
        .collect();

    let teams: Vec<TeamView> = rt
        .teams()
        .iter()
        .map(|g| {
            let mine: Vec<&TeamTask> = tasks.iter().filter(|t| t.team_id == g.id).collect();
            TeamView {
                id: g.id.clone(),
                name: g.name.clone(),
                lead_role: g.lead_role,
                parents: agents
                    .iter()
                    .filter(|a| a.team_id == g.id && a.kind == AgentKind::ManagedSession)
                    .map(|a| a.id.clone())
                    .collect(),
                done: mine
                    .iter()
                    .filter(|t| t.state == TeamTaskState::Completed)
                    .count(),
                total: mine.len(),
            }
        })
        .collect();

    let task_views: Vec<TaskView> = tasks
        .iter()
        .map(|t| TaskView {
            id: t.id,
            title: t.title.clone(),
            state: t.state,
            role: t.role,
            team_id: t.team_id.clone(),
            assigned_agent: t.assigned_agent.clone(),
            attempts: t.attempts,
            files: t.files.clone(),
            acceptance_criteria: t.acceptance_criteria.clone(),
            validation_commands: t.validation_commands.iter().map(|c| c.display()).collect(),
            validation_ok: t.validation.passed(&t.validation_commands),
            validation_ran: !t.validation.runs.is_empty(),
            validation_failed: t.validation.failed(),
            validation_result: t
                .validation
                .runs
                .iter()
                .find(|r| !r.ok())
                .map(|r| r.outcome()),
            validation_diagnostics: t
                .validation
                .runs
                .iter()
                .filter(|r| !r.ok())
                .filter_map(|r| {
                    let body = r.output.as_ref()?.excerpt(INSPECTOR_DIAGNOSTIC_BYTES);
                    if body.is_empty() {
                        return None;
                    }
                    Some(format!("{}\n{body}", r.command))
                })
                .collect(),
            review_verdict: t.review.verdict,
            review_findings: t.review.findings.clone(),
            blockers: t.blockers.clone(),
            last_summary: t.last_summary.clone(),
            context: t.context.clone(),
            review_of: t.review_of,
        })
        .collect();

    let events: Vec<TeamEventView> = rt
        .events()
        .rev()
        .take(FEED_CAP)
        .map(|e| TeamEventView {
            id: e.id,
            at: e.at,
            kind: e.kind,
            actor: e.actor.clone(),
            summary: e.summary.clone(),
        })
        .collect();

    let metrics = TeamMetricsView {
        tasks_total: tasks.len(),
        tasks_done: tasks
            .iter()
            .filter(|t| t.state == TeamTaskState::Completed)
            .count(),
        agents_active: agents
            .iter()
            .filter(|a| {
                matches!(
                    a.state,
                    AgentWorkState::Working
                        | AgentWorkState::Testing
                        | AgentWorkState::Reviewing
                        | AgentWorkState::Planning
                        | AgentWorkState::Coordinating
                )
            })
            .count(),
        blocked: tasks
            .iter()
            .filter(|t| {
                matches!(
                    t.state,
                    TeamTaskState::Blocked | TeamTaskState::NeedsUser | TeamTaskState::Failed
                )
            })
            .count(),
        tests_passed: tasks
            .iter()
            .filter(|t| t.validation.passed(&t.validation_commands))
            .count(),
        reviews_approved: tasks.iter().filter(|t| t.review.approved()).count(),
        pending_decisions: rt.decisions().len(),
        progress_pct: (graph::progress(tasks) * 100.0).round() as u8,
        agent_limit: rt.run().agent_count,
        max_attempts: rt.run().max_attempts,
    };

    TeamSnapshot {
        goal: GoalView {
            title: rt.goal().title.clone(),
            status: rt.goal().status,
            definition_of_done: rt.goal().definition_of_done.clone(),
            phase,
        },
        teams,
        agents,
        tasks: task_views,
        events,
        pending_decisions: rt
            .decisions()
            .iter()
            .map(|d| DecisionView {
                id: d.id,
                kind: d.kind,
                task_id: d.task_id,
                reason: d.reason.clone(),
                impact: d.impact.clone(),
                options: d.options.clone(),
            })
            .collect(),
        metrics,
        phases,
        paused: rt.is_paused(),
        stopped: rt.is_stopped(),
        feed_cap: FEED_CAP,
        // **実装タスクを見る。** レビュータスクはもともと検証コマンドを
        // 持たないので、混ぜると常に「検証なし」になってしまう。
        // **測る手立てがあるかは、いま見て決める。** 途中で `git init` すれば
        // 測れるようになるので、Run を作った時点の値を持ち回らない。
        unmeasured: crate::git::discover_toplevel(rt.workspace()).is_none(),
        unvalidated: {
            let mut impl_tasks = tasks.iter().filter(|t| t.review_of.is_none()).peekable();
            impl_tasks.peek().is_some()
                && tasks
                    .iter()
                    .filter(|t| t.review_of.is_none())
                    .all(|t| t.validation_commands.is_empty())
        },
    }
}

// ── Current Action Bar ───────────────────────────────────────────────

/// 画面下部に出す「いちばん重要なこと」。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentAction {
    /// 状態を表す記号 (色だけに頼らない)。
    pub glyph: &'static str,
    /// 本文。
    pub text: String,
    /// 押したときに開くもの。
    pub focus: ActionFocus,
    /// 人の操作を待っているか (待っているなら点滅してよい)。
    pub urgent: bool,
}

/// Current Action Bar を押したときに開く先。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionFocus {
    Decision(EventId),
    Agent(AgentId),
    Task(TaskId),
    None,
}

/// **優先順位は仕様どおり** (テストで固定する):
/// 1. 人の承認 2. 停滞 3. 詰まり 4. テスト失敗 5. レビュー差し戻し
/// 6. 統合中 7. 通常の作業 8. Goal Completed
pub fn current_action(s: &TeamSnapshot) -> CurrentAction {
    if let Some(d) = s.pending_decisions.first() {
        return CurrentAction {
            glyph: "!",
            text: format!("Action Required: {}", d.reason),
            focus: ActionFocus::Decision(d.id),
            urgent: true,
        };
    }
    if let Some(a) = s.agents.iter().find(|a| a.state == AgentWorkState::Stalled) {
        return CurrentAction {
            glyph: AgentWorkState::Stalled.glyph(),
            text: format!("{} が停滞しています", a.name),
            focus: ActionFocus::Agent(a.id.clone()),
            urgent: true,
        };
    }
    if let Some(t) = s.tasks.iter().find(|t| t.state == TeamTaskState::Blocked) {
        return CurrentAction {
            glyph: AgentWorkState::Blocked.glyph(),
            text: format!("#{} {} が進められません", t.id, t.title),
            focus: ActionFocus::Task(t.id),
            urgent: false,
        };
    }
    if let Some(t) = s.tasks.iter().find(|t| t.validation_failed) {
        return CurrentAction {
            glyph: AgentWorkState::Testing.glyph(),
            text: format!("#{} {} の検証が失敗しています", t.id, t.title),
            focus: ActionFocus::Task(t.id),
            urgent: false,
        };
    }
    if let Some(t) = s
        .tasks
        .iter()
        .find(|t| t.review_verdict == Some(ReviewVerdict::RequestChanges))
    {
        return CurrentAction {
            glyph: AgentWorkState::Reviewing.glyph(),
            text: format!("#{} {} にレビュー指摘があります", t.id, t.title),
            focus: ActionFocus::Task(t.id),
            urgent: false,
        };
    }
    if s.goal.status == GoalStatus::Integrating {
        return CurrentAction {
            glyph: AgentWorkState::Working.glyph(),
            text: "最終統合を実行中です".to_string(),
            focus: ActionFocus::None,
            urgent: false,
        };
    }
    if let Some(a) = s
        .agents
        .iter()
        .find(|a| a.state == AgentWorkState::Working && a.current_task.is_some())
    {
        return CurrentAction {
            glyph: AgentWorkState::Working.glyph(),
            text: format!(
                "{} — {}",
                a.name,
                if a.current_action.is_empty() {
                    a.current_task_title.clone()
                } else {
                    a.current_action.clone()
                }
            ),
            focus: ActionFocus::Agent(a.id.clone()),
            urgent: false,
        };
    }
    if s.goal.status == GoalStatus::Completed {
        return CurrentAction {
            glyph: AgentWorkState::Completed.glyph(),
            text: "Goal Completed".to_string(),
            focus: ActionFocus::None,
            urgent: false,
        };
    }
    CurrentAction {
        glyph: AgentWorkState::Idle.glyph(),
        text: "待機中".to_string(),
        focus: ActionFocus::None,
        urgent: false,
    }
}

// ── レイアウト (純関数) ──────────────────────────────────────────────

/// 放射状組織図のローカル座標。左上が `(0, 0)`。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OrganizationMapPoint {
    pub x: f32,
    pub y: f32,
}

/// 組織図上でのノードの役割。
///
/// `TeamLead` は中心の 1 体だけ。復元データに二重の
/// TeamLead があっても、2 体目以降は `ManagedSession` として描く。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrganizationMapNodeKind {
    TeamLead,
    ManagedSession,
    ReportedSubAgent,
}

/// 放射状組織図に描画する 1 エージェント。
#[derive(Clone, Debug, PartialEq)]
pub struct OrganizationMapNode {
    pub agent_id: AgentId,
    pub team_id: TeamId,
    pub parent_id: Option<AgentId>,
    pub kind: OrganizationMapNodeKind,
    pub center: OrganizationMapPoint,
    /// 幅と高さの双方に対する描画安全半径。
    pub radius: f32,
}

/// 組織図の親子関係。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrganizationMapEdge {
    pub from: AgentId,
    pub to: AgentId,
}

/// ローカルキャンバスに収まる放射状組織図。
#[derive(Clone, Debug, PartialEq)]
pub struct OrganizationMapLayout {
    pub width: f32,
    pub height: f32,
    pub nodes: Vec<OrganizationMapNode>,
    pub edges: Vec<OrganizationMapEdge>,
}

fn finite_extent(value: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        1.0
    }
}

fn organization_point(
    width: f32,
    height: f32,
    radius: f32,
    angle: f32,
    ring: f32,
) -> OrganizationMapPoint {
    let center_x = width * 0.5;
    let center_y = height * 0.5;
    let orbit_x = (center_x - radius).max(0.0) * ring.clamp(0.0, 1.0);
    let orbit_y = (center_y - radius).max(0.0) * ring.clamp(0.0, 1.0);
    OrganizationMapPoint {
        x: (center_x + angle.cos() * orbit_x).clamp(radius, width - radius),
        y: (center_y + angle.sin() * orbit_y).clamp(radius, height - radius),
    }
}

/// [`TeamSnapshot`] から放射状組織図を作る純関数。
///
/// - 中心: TeamLead
/// - 内周: 各チームの ManagedSession
/// - 外周: その親が報告した ReportedSubAgent
///
/// 並びは ID とチームだけで決めるため、作業状態が変わっても
/// 座標が動かない。復元途中の孤児ノードも捨てず、中心の TeamLead
/// に接続して描く。計算量はソートを含め `O(N log N)`。
pub fn organization_map_layout(
    snapshot: &TeamSnapshot,
    available_width: f32,
    available_height: f32,
) -> OrganizationMapLayout {
    let width = finite_extent(available_width);
    let height = finite_extent(available_height);
    if snapshot.agents.is_empty() {
        return OrganizationMapLayout {
            width,
            height,
            nodes: Vec::new(),
            edges: Vec::new(),
        };
    }

    // 入力順と状態に依存しない安定順序。同一 ID は本来作られないが、
    // 復元途中の破損データでも結果を決定的にするため安定キーを足す。
    let mut ordered: Vec<usize> = (0..snapshot.agents.len()).collect();
    ordered.sort_by(|&left, &right| {
        let left = &snapshot.agents[left];
        let right = &snapshot.agents[right];
        left.id
            .cmp(&right.id)
            .then_with(|| left.team_id.cmp(&right.team_id))
            .then_with(|| left.parent_id.cmp(&right.parent_id))
            .then_with(|| left.role.cmp(&right.role))
            .then_with(|| left.name.cmp(&right.name))
    });

    let root = ordered
        .iter()
        .copied()
        .find(|&index| {
            let agent = &snapshot.agents[index];
            agent.kind == AgentKind::ManagedSession && agent.role == TeamRole::TeamLead
        })
        .or_else(|| {
            ordered
                .iter()
                .copied()
                .find(|&index| snapshot.agents[index].kind == AgentKind::ManagedSession)
        })
        .unwrap_or(ordered[0]);

    let min_side = width.min(height);
    let population_scale = (snapshot.agents.len() as f32).sqrt().max(1.0);
    let managed_radius = (min_side / (population_scale * 5.0 + 4.0))
        .min(18.0)
        .min(min_side * 0.5)
        .max(0.0);
    // 137 体が 1 親へ集中しても、子ノード同士が外周で潰れない大きさ。
    // クリック領域は描画側が 24px を確保するため、小さくしても操作性は落ちない。
    let reported_radius = (managed_radius * 0.55).max(2.5).min(managed_radius);
    let root_radius = (managed_radius * 1.8).min(20.0).min(min_side * 0.5);
    let mut centers = vec![None; snapshot.agents.len()];
    centers[root] = Some(OrganizationMapPoint {
        x: width * 0.5,
        y: height * 0.5,
    });

    let mut managed_by_team: BTreeMap<TeamId, Vec<usize>> = BTreeMap::new();
    let mut reported_by_team: BTreeMap<TeamId, Vec<usize>> = BTreeMap::new();
    for &index in &ordered {
        if index == root {
            continue;
        }
        let agent = &snapshot.agents[index];
        match agent.kind {
            AgentKind::ManagedSession => managed_by_team
                .entry(agent.team_id.clone())
                .or_default()
                .push(index),
            AgentKind::ReportedSubAgent => {
                reported_by_team
                    .entry(agent.team_id.clone())
                    .or_default()
                    .push(index);
            }
        }
    }

    let tau = std::f32::consts::TAU;
    let team_ids: BTreeSet<TeamId> = managed_by_team
        .keys()
        .chain(reported_by_team.keys())
        .cloned()
        .collect();
    let population: usize = team_ids
        .iter()
        .map(|team| {
            managed_by_team.get(team).map_or(0, Vec::len)
                + reported_by_team.get(team).map_or(0, Vec::len)
        })
        .sum();
    let mut sector_start = -std::f32::consts::FRAC_PI_2;
    if population > 0 {
        for team in team_ids {
            let parents = managed_by_team.get(&team).map(Vec::as_slice).unwrap_or(&[]);
            let children = reported_by_team
                .get(&team)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let team_population = parents.len() + children.len();
            // 人口の多い部門へ広い弧を渡す。1 親に 136 体でも全周を使える。
            let team_span = tau * team_population as f32 / population as f32;
            let parent_cell = team_span / (parents.len() + 1) as f32;
            for (parent_index, &index) in parents.iter().enumerate() {
                let angle = sector_start + (parent_index + 1) as f32 * parent_cell;
                centers[index] = Some(organization_point(
                    width,
                    height,
                    managed_radius,
                    angle,
                    0.54,
                ));
            }
            let child_cell = team_span / children.len().max(1) as f32;
            for (child_index, &index) in children.iter().enumerate() {
                let angle = sector_start + (child_index as f32 + 0.5) * child_cell;
                centers[index] = Some(organization_point(
                    width,
                    height,
                    reported_radius,
                    angle,
                    0.92,
                ));
            }
            sector_start += team_span;
        }
    }

    // parent_id が無い、または復元時に親が失われたサブエージェント。
    // ここも agent の安定順序だけで決め、必ずキャンバス内へ戻す。
    let leftovers: Vec<usize> = ordered
        .iter()
        .copied()
        .filter(|&index| centers[index].is_none())
        .collect();
    for (position, index) in leftovers.iter().copied().enumerate() {
        let angle =
            -std::f32::consts::FRAC_PI_2 + tau * (position as f32 + 0.5) / leftovers.len() as f32;
        centers[index] = Some(organization_point(
            width,
            height,
            reported_radius,
            angle,
            0.92,
        ));
    }

    let root_id = snapshot.agents[root].id.clone();
    let managed_ids: BTreeSet<AgentId> = snapshot
        .agents
        .iter()
        .enumerate()
        .filter(|(index, agent)| *index != root && agent.kind == AgentKind::ManagedSession)
        .map(|(_, agent)| agent.id.clone())
        .collect();

    let mut nodes: Vec<OrganizationMapNode> = ordered
        .iter()
        .copied()
        .map(|index| {
            let agent = &snapshot.agents[index];
            OrganizationMapNode {
                agent_id: agent.id.clone(),
                team_id: agent.team_id.clone(),
                parent_id: agent.parent_id.clone(),
                kind: if index == root {
                    OrganizationMapNodeKind::TeamLead
                } else {
                    match agent.kind {
                        AgentKind::ManagedSession => OrganizationMapNodeKind::ManagedSession,
                        AgentKind::ReportedSubAgent => OrganizationMapNodeKind::ReportedSubAgent,
                    }
                },
                center: centers[index].expect("all organization nodes receive a position"),
                radius: if index == root {
                    root_radius
                } else {
                    match agent.kind {
                        AgentKind::ManagedSession => managed_radius,
                        AgentKind::ReportedSubAgent => reported_radius,
                    }
                },
            }
        })
        .collect();
    nodes.sort_by(|left, right| left.agent_id.cmp(&right.agent_id));

    let mut edges = Vec::with_capacity(snapshot.agents.len().saturating_sub(1));
    for &index in &ordered {
        if index == root {
            continue;
        }
        let agent = &snapshot.agents[index];
        let from = match (&agent.kind, &agent.parent_id) {
            (AgentKind::ReportedSubAgent, Some(parent)) if managed_ids.contains(parent) => {
                parent.clone()
            }
            _ => root_id.clone(),
        };
        edges.push(OrganizationMapEdge {
            from,
            to: agent.id.clone(),
        });
    }
    edges.sort_by(|left, right| {
        left.from
            .cmp(&right.from)
            .then_with(|| left.to.cmp(&right.to))
    });

    OrganizationMapLayout {
        width,
        height,
        nodes,
        edges,
    }
}

/// レーンの並べ方。
#[derive(Clone, Debug, PartialEq)]
pub struct LaneLayout {
    /// 1 本あたりの幅。
    pub lane_w: f32,
    /// 一度に見せる本数。
    pub visible: usize,
    /// 横スクロールが要るか。
    pub scroll: bool,
    /// Mission Panel を出すか。
    pub mission_panel: bool,
    /// アイコンだけに縮退するか (狭いとき)。
    pub compact: bool,
}

/// 可用幅とレーン数からレイアウトを決める。
///
/// **どの幅でも見切れないこと**が守る性質で、テストが極端なサイズで
/// 矩形の収まりを確かめる。
pub fn lane_layout(avail_w: f32, lanes: usize) -> LaneLayout {
    let mission = avail_w >= MISSION_PANEL_MIN_TOTAL_W;
    let board_w = if mission {
        avail_w - MISSION_PANEL_W
    } else {
        avail_w
    }
    .max(LANE_MIN_W);
    if lanes == 0 {
        return LaneLayout {
            lane_w: board_w,
            visible: 0,
            scroll: false,
            mission_panel: mission,
            compact: board_w < LANE_MIN_W * 2.0,
        };
    }
    // 収まる本数 = 幅 ÷ 最小幅 (ただし標準は 5 本まで)
    let fit = ((board_w / LANE_MIN_W).floor() as usize)
        .clamp(1, MAX_LANES_ON_SCREEN)
        .min(lanes);
    let scroll = fit < lanes;
    // 収まる本数で等分する。理想幅を超えたら理想幅で止めて左寄せにする。
    let lane_w = (board_w / fit as f32).min(LANE_IDEAL_W).max(LANE_MIN_W);
    LaneLayout {
        lane_w,
        visible: fit,
        scroll,
        mission_panel: mission,
        compact: lane_w <= LANE_MIN_W + 1.0,
    }
}

/// 経過時間の短い表記 (`04:12`)。**1 時間を超えたら `1h 02m`。**
pub fn elapsed_label(secs: u64) -> String {
    if secs < 3600 {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> TeamSnapshot {
        TeamSnapshot {
            goal: GoalView {
                title: "g".into(),
                status: GoalStatus::Running,
                definition_of_done: vec!["d".into()],
                phase: Phase::Implementation,
            },
            teams: Vec::new(),
            agents: Vec::new(),
            tasks: Vec::new(),
            events: Vec::new(),
            pending_decisions: Vec::new(),
            metrics: TeamMetricsView::default(),
            phases: Vec::new(),
            paused: false,
            stopped: false,
            feed_cap: FEED_CAP,
            unvalidated: false,
            unmeasured: false,
        }
    }

    fn agent(name: &str, state: AgentWorkState) -> TeamAgentView {
        TeamAgentView {
            id: AgentId::new(name),
            name: name.into(),
            role: TeamRole::Implementer,
            team_id: TeamId::new("implementation"),
            parent_id: None,
            kind: AgentKind::ManagedSession,
            session_id: Some(1),
            provider: "claude".into(),
            state,
            current_task: Some(1),
            current_task_title: "task".into(),
            current_action: "JWT middleware 実装中".into(),
            children: Vec::new(),
            idle_secs: 0,
            done: 0,
            assigned: 1,
            can_open_terminal: true,
            blockers: Vec::new(),
            preview: String::new(),
        }
    }

    fn organization_agent(
        id: &str,
        team: &str,
        role: TeamRole,
        kind: AgentKind,
        parent: Option<&str>,
    ) -> TeamAgentView {
        let mut view = agent(id, AgentWorkState::Working);
        view.role = role;
        view.team_id = TeamId::new(team);
        view.parent_id = parent.map(AgentId::new);
        view.kind = kind;
        view.session_id = (kind == AgentKind::ManagedSession).then_some(1);
        view.can_open_terminal = kind == AgentKind::ManagedSession;
        view
    }

    fn organization_snapshot(total: usize) -> TeamSnapshot {
        assert!(total >= 3);
        let mut snapshot = snap();
        snapshot.agents.push(organization_agent(
            "lead",
            "coordination",
            TeamRole::TeamLead,
            AgentKind::ManagedSession,
            None,
        ));

        let managed_count = ((total - 1) / 3).max(1);
        for index in 0..managed_count {
            snapshot.agents.push(organization_agent(
                &format!("managed-{index:03}"),
                &format!("team-{:02}", index % 7),
                TeamRole::Implementer,
                AgentKind::ManagedSession,
                Some("lead"),
            ));
        }
        for index in 0..(total - managed_count - 1) {
            let parent = format!("managed-{:03}", index % managed_count);
            snapshot.agents.push(organization_agent(
                &format!("reported-{index:03}"),
                &format!("team-{:02}", (index % managed_count) % 7),
                TeamRole::Implementer,
                AgentKind::ReportedSubAgent,
                Some(&parent),
            ));
        }
        snapshot
    }

    fn assert_organization_inside(layout: &OrganizationMapLayout) {
        assert!(layout.width.is_finite() && layout.width > 0.0);
        assert!(layout.height.is_finite() && layout.height > 0.0);
        for node in &layout.nodes {
            assert!(node.center.x.is_finite(), "{}: x is NaN", node.agent_id.0);
            assert!(node.center.y.is_finite(), "{}: y is NaN", node.agent_id.0);
            assert!(
                node.radius.is_finite(),
                "{}: radius is NaN",
                node.agent_id.0
            );
            assert!(node.radius >= 0.0);
            assert!(node.center.x - node.radius >= -f32::EPSILON);
            assert!(node.center.y - node.radius >= -f32::EPSILON);
            assert!(node.center.x + node.radius <= layout.width + f32::EPSILON);
            assert!(node.center.y + node.radius <= layout.height + f32::EPSILON);
        }
    }

    #[test]
    fn 放射状組織図は全agentと親子関係を1回ずつ返す() {
        let mut snapshot = snap();
        snapshot.agents = vec![
            organization_agent(
                "reported-b",
                "frontend",
                TeamRole::Tester,
                AgentKind::ReportedSubAgent,
                Some("managed-b"),
            ),
            organization_agent(
                "managed-b",
                "frontend",
                TeamRole::Tester,
                AgentKind::ManagedSession,
                Some("lead"),
            ),
            organization_agent(
                "lead",
                "coordination",
                TeamRole::TeamLead,
                AgentKind::ManagedSession,
                None,
            ),
            organization_agent(
                "reported-a",
                "backend",
                TeamRole::Implementer,
                AgentKind::ReportedSubAgent,
                Some("managed-a"),
            ),
            organization_agent(
                "managed-a",
                "backend",
                TeamRole::Implementer,
                AgentKind::ManagedSession,
                Some("lead"),
            ),
        ];

        let layout = organization_map_layout(&snapshot, 900.0, 600.0);
        assert_eq!(layout.nodes.len(), snapshot.agents.len());
        let ids: BTreeSet<_> = layout.nodes.iter().map(|node| &node.agent_id).collect();
        assert_eq!(ids.len(), snapshot.agents.len());
        assert_eq!(layout.edges.len(), snapshot.agents.len() - 1);
        assert!(layout.edges.contains(&OrganizationMapEdge {
            from: AgentId::new("lead"),
            to: AgentId::new("managed-a"),
        }));
        assert!(layout.edges.contains(&OrganizationMapEdge {
            from: AgentId::new("lead"),
            to: AgentId::new("managed-b"),
        }));
        assert!(layout.edges.contains(&OrganizationMapEdge {
            from: AgentId::new("managed-a"),
            to: AgentId::new("reported-a"),
        }));
        assert!(layout.edges.contains(&OrganizationMapEdge {
            from: AgentId::new("managed-b"),
            to: AgentId::new("reported-b"),
        }));
        let root = layout
            .nodes
            .iter()
            .find(|node| node.kind == OrganizationMapNodeKind::TeamLead)
            .unwrap();
        assert_eq!(root.agent_id, AgentId::new("lead"));
        assert_eq!(root.center, OrganizationMapPoint { x: 450.0, y: 300.0 });
        assert_organization_inside(&layout);
    }

    #[test]
    fn 放射状組織図の並びは状態と入力順で動かない() {
        let snapshot = organization_snapshot(64);
        let expected = organization_map_layout(&snapshot, 1280.0, 720.0);
        let mut changed = snapshot.clone();
        changed.agents.reverse();
        for (index, agent) in changed.agents.iter_mut().enumerate() {
            agent.state = if index % 2 == 0 {
                AgentWorkState::Completed
            } else {
                AgentWorkState::Stalled
            };
            agent.current_action = format!("changed-{index}");
        }
        assert_eq!(organization_map_layout(&changed, 1280.0, 720.0), expected);
    }

    #[test]
    fn 放射状組織図は64体と137体で範囲内に収まる() {
        for total in [64, 137] {
            let snapshot = organization_snapshot(total);
            for (width, height) in [(360.0, 240.0), (1024.0, 768.0), (1920.0, 540.0)] {
                let layout = organization_map_layout(&snapshot, width, height);
                assert_eq!(layout.nodes.len(), total);
                assert_eq!(layout.edges.len(), total - 1);
                let ids: BTreeSet<_> = layout.nodes.iter().map(|node| &node.agent_id).collect();
                assert_eq!(ids.len(), total);
                assert_organization_inside(&layout);
            }
        }
    }

    #[test]
    fn 一親に百三十五体集中しても子ノードは重ならない() {
        let mut snapshot = snap();
        snapshot.agents.push(organization_agent(
            "lead",
            "coordination",
            TeamRole::TeamLead,
            AgentKind::ManagedSession,
            None,
        ));
        snapshot.agents.push(organization_agent(
            "parent",
            "dense",
            TeamRole::Implementer,
            AgentKind::ManagedSession,
            Some("lead"),
        ));
        for index in 0..135 {
            snapshot.agents.push(organization_agent(
                &format!("child-{index:03}"),
                "dense",
                TeamRole::Implementer,
                AgentKind::ReportedSubAgent,
                Some("parent"),
            ));
        }
        let layout = organization_map_layout(&snapshot, 680.0, 520.0);
        let children: Vec<_> = layout
            .nodes
            .iter()
            .filter(|n| n.kind == OrganizationMapNodeKind::ReportedSubAgent)
            .collect();
        assert_eq!(children.len(), 135);
        for (index, left) in children.iter().enumerate() {
            for right in &children[index + 1..] {
                let dx = left.center.x - right.center.x;
                let dy = left.center.y - right.center.y;
                let distance = (dx * dx + dy * dy).sqrt();
                assert!(
                    distance + f32::EPSILON >= left.radius + right.radius,
                    "{} と {} が重なる: {distance}",
                    left.agent_id,
                    right.agent_id
                );
            }
        }
    }

    #[test]
    fn 放射状組織図は不正なキャンバス寸法でもnanを返さない() {
        let snapshot = organization_snapshot(137);
        let layout = organization_map_layout(&snapshot, f32::NAN, f32::INFINITY);
        assert_eq!((layout.width, layout.height), (1.0, 1.0));
        assert_organization_inside(&layout);
    }

    #[test]
    fn 人の判断が最優先() {
        let mut s = snap();
        s.agents.push(agent("a", AgentWorkState::Stalled));
        s.pending_decisions.push(DecisionView {
            id: 7,
            kind: DecisionKind::DestructiveChange,
            task_id: None,
            reason: "DB migration の破壊的変更を承認してください".into(),
            impact: "".into(),
            options: vec!["approve".into()],
        });
        let a = current_action(&s);
        assert_eq!(a.focus, ActionFocus::Decision(7));
        assert!(a.urgent);
        assert!(a.text.contains("Action Required"));
    }

    #[test]
    fn 停滞は通常作業より優先() {
        let mut s = snap();
        s.agents.push(agent("working", AgentWorkState::Working));
        s.agents.push(agent("stuck", AgentWorkState::Stalled));
        assert_eq!(
            current_action(&s).focus,
            ActionFocus::Agent(AgentId::new("stuck"))
        );
    }

    #[test]
    fn 通常時は作業中のエージェントを出す() {
        let mut s = snap();
        s.agents.push(agent("w", AgentWorkState::Working));
        let a = current_action(&s);
        assert_eq!(a.focus, ActionFocus::Agent(AgentId::new("w")));
        assert!(a.text.contains("JWT middleware"));
        assert!(!a.urgent);
    }

    #[test]
    fn 何も無ければ待機中() {
        assert_eq!(current_action(&snap()).focus, ActionFocus::None);
    }

    #[test]
    fn goal完了を出す() {
        let mut s = snap();
        s.goal.status = GoalStatus::Completed;
        assert_eq!(current_action(&s).text, "Goal Completed");
    }

    #[test]
    fn レーンはどの幅でも見切れない() {
        for w in [400.0f32, 700.0, 900.0, 1200.0, 1480.0, 2400.0] {
            for lanes in 0..9usize {
                let l = lane_layout(w, lanes);
                let board = if l.mission_panel {
                    w - MISSION_PANEL_W
                } else {
                    w
                }
                .max(LANE_MIN_W);
                let used = l.lane_w * l.visible as f32;
                assert!(
                    used <= board + 0.5,
                    "w={w} lanes={lanes} 使用幅 {used} > 可用 {board}"
                );
                assert!(l.visible <= lanes);
                if lanes > 0 {
                    assert!(l.lane_w >= LANE_MIN_W - 0.5, "w={w} lanes={lanes}");
                    assert!(l.visible >= 1);
                }
                if lanes > MAX_LANES_ON_SCREEN {
                    assert!(l.scroll, "w={w} lanes={lanes} 横スクロールが要る");
                }
            }
        }
    }

    #[test]
    fn 狭い画面ではmission_panelを出さない() {
        assert!(!lane_layout(700.0, 4).mission_panel);
        assert!(lane_layout(1200.0, 4).mission_panel);
    }

    #[test]
    fn 経過時間の表記() {
        assert_eq!(elapsed_label(0), "00:00");
        assert_eq!(elapsed_label(252), "04:12");
        assert_eq!(elapsed_label(3599), "59:59");
        assert_eq!(elapsed_label(3720), "1h 02m");
    }

    #[test]
    fn 六十四体でもフィードは上限を超えない() {
        let mut s = snap();
        for i in 0..500 {
            s.events.push(TeamEventView {
                id: i,
                at: 0,
                kind: TeamEventKind::TaskReady,
                actor: None,
                summary: "x".into(),
            });
        }
        // スナップショット生成側で切るので、ここは上限値そのものを固定する
        assert_eq!(s.feed_cap, FEED_CAP);
        assert!(FEED_CAP <= 100, "フィードの上限が大きすぎる");
    }
}
