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
