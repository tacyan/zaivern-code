//! 🏛 AI Organization Board — Team 画面の描画。
//!
//! ## この層がしないこと
//!
//! * プロセス起動・停止 / LLM 呼び出し / ファイル保存 / 重い解析
//! * 状態の書き換え (**第 2 の真実を作らない**)
//!
//! 読むのは [`TeamSnapshot`](super::view_model::TeamSnapshot) だけで、
//! 操作は [`BoardAction`](super::panel::BoardAction) として返す。
//!
//! ## レイアウトの約束 (CLAUDE.md の UI 原則)
//!
//! * **どの幅でも見切れない** — レーン幅は [`super::view_model::lane_layout`]
//!   が決め、入り切らないぶんは横スクロールへ逃がす
//! * **空白は作らない** — 中身の無いセクションは高さを 1px も取らない。
//!   空状態は利用可能領域の**中央**に 1 枚のカードで出す
//! * **点滅は停滞と緊急承認だけ** — 常時アニメーションはバッテリーのバグ
//! * **色だけに頼らない** — 状態は必ず記号 ([`AgentWorkState::glyph`]) を伴う

use eframe::egui;
use egui::{Align2, RichText};

use crate::i18n::{tr, trf};
use crate::theme::Theme;

use super::graph::{Phase, PhaseStatus};
use super::model::*;
use super::panel::{BoardAction, BoardTab, NewRunForm, RestorePrompt};
use super::view_model::{
    self, current_action, ActionFocus, TeamAgentView, TeamSnapshot, MISSION_PANEL_W,
};

/// 子エージェント行を 1 枚のカードに出す上限。**超えたぶんは「他 N 件」**
/// (64 体でも縦に伸び続けないための線)。
pub const CHILD_ROWS_MAX: usize = 6;
/// Activity Feed に描く行数。
pub const FEED_ROWS: usize = 12;

/// 点滅の周期 (秒)。**停滞と緊急承認だけ**が対象で、それ以外は 1 回も点かない
/// (常時アニメーションはバッテリーのバグ — 設計原則 3)。
pub const BLINK_PERIOD: f64 = 1.0;

/// 状態の記号を、必要なときだけ点滅させて描く。
///
/// 点滅させるのは [`AgentWorkState::may_blink`] が真のものだけ。点滅させる
/// フレームでだけ再描画を頼むので、静かな盤面ではタイマーが 1 本も回らない。
fn glyph_label(ui: &mut egui::Ui, theme: &Theme, s: AgentWorkState) {
    let col = state_color(theme, s);
    if !s.may_blink() {
        ui.label(RichText::new(s.glyph()).color(col));
        return;
    }
    let t = ui.input(|i| i.time);
    let on = (t / (BLINK_PERIOD / 2.0)) as i64 % 2 == 0;
    let col = if on { col } else { theme.text_dim };
    ui.label(RichText::new(s.glyph()).color(col));
    crate::perf::repaint_after(
        ui.ctx(),
        std::time::Duration::from_secs_f64(BLINK_PERIOD / 2.0),
        "team-blink",
    );
}

/// 状態に対応する色。**色は補助**で、意味は記号が持つ。
fn state_color(theme: &Theme, s: AgentWorkState) -> egui::Color32 {
    match s {
        AgentWorkState::Working | AgentWorkState::Coordinating => theme.accent,
        AgentWorkState::Testing => theme.accent_soft,
        AgentWorkState::Reviewing => theme.accent_soft,
        AgentWorkState::WaitingApproval => theme.warn,
        AgentWorkState::Blocked => theme.warn,
        AgentWorkState::Stalled => theme.err,
        AgentWorkState::Completed => theme.ok,
        AgentWorkState::Exited => theme.text_dim,
        _ => theme.text_dim,
    }
}

/// フェーズの表示名 (i18n の ID を通す)。
fn phase_label(p: Phase) -> String {
    // **`tr(match …)` と書かない。** `zai i18n missing` も番人テストも
    // `tr("…")` の**素のリテラル**しか辿れないので、その形にすると
    // 抜けても誰も何も言わないまま画面に ID が出る (CLAUDE.md の実例)。
    match p {
        Phase::GoalAnalysis => tr("team.phase.goal_analysis"),
        Phase::Architecture => tr("team.phase.architecture"),
        Phase::Implementation => tr("team.phase.implementation"),
        Phase::Review => tr("team.phase.review"),
        Phase::Integration => tr("team.phase.integration"),
        Phase::FinalValidation => tr("team.phase.final_validation"),
    }
}

fn phase_status_label(s: PhaseStatus) -> String {
    match s {
        PhaseStatus::Waiting => tr("team.phase_status.waiting"),
        PhaseStatus::Running => tr("team.phase_status.running"),
        PhaseStatus::Done => tr("team.phase_status.done"),
    }
}

fn goal_status_label(s: GoalStatus) -> String {
    match s {
        GoalStatus::Planning => tr("team.goal.planning"),
        GoalStatus::Ready => tr("team.goal.ready"),
        GoalStatus::Running => tr("team.goal.running"),
        GoalStatus::Paused => tr("team.goal.paused"),
        GoalStatus::Blocked => tr("team.goal.blocked"),
        GoalStatus::Reviewing => tr("team.goal.reviewing"),
        GoalStatus::Integrating => tr("team.goal.integrating"),
        GoalStatus::Completed => tr("team.goal.completed"),
        GoalStatus::Failed => tr("team.goal.failed"),
        GoalStatus::NeedsUser => tr("team.goal.needs_user"),
    }
}

fn task_state_label(s: TeamTaskState) -> String {
    match s {
        TeamTaskState::Pending => tr("team.task.pending"),
        TeamTaskState::Ready => tr("team.task.ready"),
        TeamTaskState::Assigned => tr("team.task.assigned"),
        TeamTaskState::Running => tr("team.task.running"),
        TeamTaskState::Blocked => tr("team.task.blocked"),
        TeamTaskState::Validating => tr("team.task.validating"),
        TeamTaskState::Reviewing => tr("team.task.reviewing"),
        TeamTaskState::RevisionRequired => tr("team.task.revision_required"),
        TeamTaskState::Failed => tr("team.task.failed"),
        TeamTaskState::Completed => tr("team.task.completed"),
        TeamTaskState::NeedsUser => tr("team.task.needs_user"),
    }
}

fn work_state_label(s: AgentWorkState) -> String {
    match s {
        AgentWorkState::Idle => tr("team.agent.idle"),
        AgentWorkState::Planning => tr("team.agent.planning"),
        AgentWorkState::Coordinating => tr("team.agent.coordinating"),
        AgentWorkState::Working => tr("team.agent.working"),
        AgentWorkState::Testing => tr("team.agent.testing"),
        AgentWorkState::Reviewing => tr("team.agent.reviewing"),
        AgentWorkState::WaitingApproval => tr("team.agent.waiting_approval"),
        AgentWorkState::Blocked => tr("team.agent.blocked"),
        AgentWorkState::Stalled => tr("team.agent.stalled"),
        AgentWorkState::Completed => tr("team.agent.completed"),
        AgentWorkState::Exited => tr("team.agent.exited"),
        AgentWorkState::Unknown => tr("team.agent.unknown"),
    }
}

fn role_label(r: TeamRole) -> String {
    match r {
        TeamRole::TeamLead => tr("team.role.team_lead"),
        TeamRole::Planner => tr("team.role.planner"),
        TeamRole::Architect => tr("team.role.architect"),
        TeamRole::Implementer => tr("team.role.implementer"),
        TeamRole::Tester => tr("team.role.tester"),
        TeamRole::Reviewer => tr("team.role.reviewer"),
        TeamRole::Integrator => tr("team.role.integrator"),
    }
}

fn tab_label(t: BoardTab) -> String {
    match t {
        BoardTab::Organization => tr("team.tab.organization"),
        BoardTab::Tasks => tr("team.tab.tasks"),
        BoardTab::Terminals => tr("team.tab.terminals"),
        BoardTab::Timeline => tr("team.tab.timeline"),
    }
}

/// 長い文字列を省略する (全文はホバーで出す)。
fn ellipsis(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let cut: String = s.chars().take(n.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// **エージェントの出力を egui のマークアップとして解釈しない。**
///
/// `RichText` は装飾を持たないが、改行やタブが混ざると行が崩れるので、
/// 制御文字を潰して 1 行に畳む。
fn plain(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Team 画面を全画面のオーバーレイとして描く。
///
/// **閉じているときは 1 命令も走らない。**
#[allow(clippy::too_many_arguments)]
pub fn board_window(
    ctx: &egui::Context,
    theme: &Theme,
    open: bool,
    snap: Option<&TeamSnapshot>,
    tab: BoardTab,
    form: &mut NewRunForm,
    restore: RestorePrompt,
    selected: Option<&AgentId>,
    notice: &str,
) -> Vec<BoardAction> {
    let mut acts = Vec::new();
    if !open {
        return acts;
    }
    let mut win_open = true;
    egui::Window::new(tr("team.window_title"))
        .open(&mut win_open)
        .collapsible(false)
        .resizable(true)
        .default_width(1180.0)
        .default_height(760.0)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            body(ui, theme, snap, tab, form, restore, selected, notice, &mut acts);
        });
    if !win_open {
        acts.push(BoardAction::Close);
    }
    acts
}

#[allow(clippy::too_many_arguments)]
fn body(
    ui: &mut egui::Ui,
    theme: &Theme,
    snap: Option<&TeamSnapshot>,
    tab: BoardTab,
    form: &mut NewRunForm,
    restore: RestorePrompt,
    selected: Option<&AgentId>,
    notice: &str,
    acts: &mut Vec<BoardAction>,
) {
    if !notice.is_empty() {
        ui.colored_label(theme.warn, plain(notice));
    }

    // ── 未完了 Run の扱い ──
    if restore != RestorePrompt::None {
        restore_card(ui, theme, restore, acts);
        return;
    }

    // ── New Team Run のフォーム ──
    if form.open {
        new_run_form(ui, theme, form, acts);
        return;
    }

    let Some(s) = snap else {
        empty_card(ui, theme, acts);
        return;
    };

    top_command_bar(ui, theme, s, acts);
    ui.separator();

    // ── タブ ──
    ui.horizontal(|ui| {
        for t in BoardTab::ALL {
            if ui.selectable_label(tab == t, tab_label(t)).clicked() {
                acts.push(BoardAction::SwitchTab(t));
            }
        }
    });
    ui.separator();

    let avail = ui.available_size();
    let layout = view_model::lane_layout(avail.x, s.teams.len());
    let bottom_h = 30.0;
    let content_h = (avail.y - bottom_h).max(120.0);

    ui.horizontal_top(|ui| {
        let board_w = if layout.mission_panel {
            (avail.x - MISSION_PANEL_W).max(view_model::LANE_MIN_W)
        } else {
            avail.x
        };
        ui.allocate_ui(egui::vec2(board_w, content_h), |ui| match tab {
            BoardTab::Organization => organization_tab(ui, theme, s, &layout, selected, acts),
            BoardTab::Tasks => tasks_tab(ui, theme, s, acts),
            BoardTab::Terminals => terminals_tab(ui, theme, s, acts),
            BoardTab::Timeline => timeline_tab(ui, theme, s),
        });
        if layout.mission_panel {
            ui.separator();
            ui.allocate_ui(egui::vec2(MISSION_PANEL_W - 12.0, content_h), |ui| {
                mission_panel(ui, theme, s, acts);
            });
        }
    });

    ui.separator();
    current_action_bar(ui, theme, s, acts);
}

// ── Top Command Bar ──────────────────────────────────────────────────

fn top_command_bar(ui: &mut egui::Ui, theme: &Theme, s: &TeamSnapshot, acts: &mut Vec<BoardAction>) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(ellipsis(&s.goal.title, 44)).color(theme.text).strong())
            .on_hover_text(&s.goal.title);
        ui.label(
            RichText::new(format!("[{}]", goal_status_label(s.goal.status)))
                .color(goal_color(theme, s.goal.status)),
        );
        ui.label(
            RichText::new(phase_label(s.goal.phase)).color(theme.text_dim),
        );
        ui.separator();
        let m = &s.metrics;
        ui.label(trf(
            "team.metrics.progress",
            &[("pct", m.progress_pct.to_string())],
        ));
        ui.label(trf(
            "team.metrics.tasks",
            &[
                ("done", m.tasks_done.to_string()),
                ("total", m.tasks_total.to_string()),
            ],
        ));
        ui.label(trf("team.metrics.agents", &[("n", m.agents_active.to_string())]));
        // **常に 0 のバッジを出さない** (CLAUDE.md: 減らせないかを先に考える)。
        if m.blocked > 0 {
            ui.label(
                RichText::new(trf("team.metrics.blocked", &[("n", m.blocked.to_string())]))
                    .color(theme.warn),
            );
        }
        if m.tests_passed > 0 {
            ui.label(
                RichText::new(trf("team.metrics.tests", &[("n", m.tests_passed.to_string())]))
                    .color(theme.ok),
            );
        }
        if m.reviews_approved > 0 {
            ui.label(trf(
                "team.metrics.reviews",
                &[("n", m.reviews_approved.to_string())],
            ));
        }
        ui.separator();
        if s.paused {
            if ui.button(tr("team.btn.resume")).clicked() {
                acts.push(BoardAction::Resume);
            }
        } else if ui.button(tr("team.btn.pause")).clicked() {
            acts.push(BoardAction::Pause);
        }
        if ui
            .button(tr("team.btn.stop"))
            .on_hover_text(tr("team.btn.stop_hint"))
            .clicked()
        {
            acts.push(BoardAction::Stop);
        }
        if s.goal.status == GoalStatus::Ready && ui.button(tr("team.btn.start")).clicked() {
            acts.push(BoardAction::Start);
        }
    });
}

fn goal_color(theme: &Theme, s: GoalStatus) -> egui::Color32 {
    match s {
        GoalStatus::Completed => theme.ok,
        GoalStatus::Failed | GoalStatus::Blocked | GoalStatus::NeedsUser => theme.err,
        GoalStatus::Paused => theme.warn,
        _ => theme.accent,
    }
}

// ── Organization タブ ────────────────────────────────────────────────

fn organization_tab(
    ui: &mut egui::Ui,
    theme: &Theme,
    s: &TeamSnapshot,
    layout: &view_model::LaneLayout,
    selected: Option<&AgentId>,
    acts: &mut Vec<BoardAction>,
) {
    // ── Team Lead (画面上部中央) ──
    if let Some(lead) = s.agents.iter().find(|a| a.role == TeamRole::TeamLead) {
        team_lead_card(ui, theme, s, lead, acts);
        ui.add_space(4.0);
    }

    if s.teams.is_empty() {
        centered_note(ui, theme, &tr("team.empty.no_lanes"));
        return;
    }

    let scroll = egui::ScrollArea::horizontal()
        // **タブごとに ID を分ける。** `ScrollArea` は `make_persistent_id`
        // 系なので、別のタブと同じ ID を使うとスクロール位置を取り合う。
        .id_salt(format!("team-scroll-{}", BoardTab::Organization.key()))
        .auto_shrink([false, false]);
    scroll.show(ui, |ui| {
        ui.horizontal_top(|ui| {
            for lane in &s.teams {
                ui.allocate_ui(egui::vec2(layout.lane_w - 8.0, ui.available_height()), |ui| {
                    lane_ui(ui, theme, s, lane, selected, layout.compact, acts);
                });
            }
        });
    });
}

fn team_lead_card(
    ui: &mut egui::Ui,
    theme: &Theme,
    s: &TeamSnapshot,
    lead: &TeamAgentView,
    acts: &mut Vec<BoardAction>,
) {
    egui::Frame::none()
        .fill(theme.panel_alt)
        .stroke(egui::Stroke::new(1.0, theme.border))
        .inner_margin(egui::Margin::symmetric(10.0, 6.0))
        .rounding(4.0)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                glyph_label(ui, theme, lead.state);
                let label = ui.selectable_label(
                    false,
                    RichText::new(format!("{} — {}", lead.name, role_label(lead.role)))
                        .color(theme.text)
                        .strong(),
                );
                if label.clicked() {
                    acts.push(BoardAction::Select(lead.id.clone()));
                }
                if !lead.provider.is_empty() {
                    ui.label(RichText::new(&lead.provider).color(theme.text_dim));
                }
                ui.label(work_state_label(lead.state));
                ui.label(trf(
                    "team.lead.teams",
                    &[("n", s.teams.len().to_string())],
                ));
                if !s.pending_decisions.is_empty() {
                    ui.label(
                        RichText::new(trf(
                            "team.lead.decisions",
                            &[("n", s.pending_decisions.len().to_string())],
                        ))
                        .color(theme.warn),
                    );
                }
                ui.label(
                    RichText::new(view_model::elapsed_label(lead.idle_secs))
                        .color(theme.text_dim),
                );
            });
            let action = if lead.current_action.is_empty() {
                phase_label(s.goal.phase)
            } else {
                plain(&lead.current_action)
            };
            ui.label(RichText::new(ellipsis(&action, 90)).color(theme.text_dim))
                .on_hover_text(action);
        });
}

fn lane_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    s: &TeamSnapshot,
    lane: &view_model::TeamView,
    selected: Option<&AgentId>,
    compact: bool,
    acts: &mut Vec<BoardAction>,
) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(ellipsis(&lane.name, 20)).color(theme.text).strong())
                .on_hover_text(&lane.name);
            // **常に 0 のバッジを出さない。**
            if lane.total > 0 {
                ui.label(
                    RichText::new(format!("{}/{}", lane.done, lane.total))
                        .color(theme.text_dim),
                );
            }
        });
        ui.separator();
        let parents: Vec<&TeamAgentView> = s
            .agents
            .iter()
            .filter(|a| a.team_id == lane.id && a.kind == AgentKind::ManagedSession)
            .collect();
        if parents.is_empty() {
            // **空のセクションで高さを稼がない。** 1 行だけ薄く出す。
            ui.label(RichText::new(tr("team.lane.no_agent")).color(theme.text_dim));
            return;
        }
        egui::ScrollArea::vertical()
            .id_salt(format!("team-lane-{}", lane.id))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for p in parents {
                    parent_card(ui, theme, s, p, selected, compact, acts);
                    ui.add_space(4.0);
                }
            });
    });
}

fn parent_card(
    ui: &mut egui::Ui,
    theme: &Theme,
    s: &TeamSnapshot,
    a: &TeamAgentView,
    selected: Option<&AgentId>,
    compact: bool,
    acts: &mut Vec<BoardAction>,
) {
    let is_sel = selected == Some(&a.id);
    egui::Frame::none()
        .fill(if is_sel { theme.panel_alt } else { theme.panel })
        .stroke(egui::Stroke::new(
            1.0,
            if is_sel { theme.accent } else { theme.border },
        ))
        .inner_margin(egui::Margin::symmetric(8.0, 6.0))
        .rounding(4.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                glyph_label(ui, theme, a.state);
                if ui
                    .selectable_label(is_sel, RichText::new(ellipsis(&a.name, 18)).color(theme.text))
                    .clicked()
                {
                    acts.push(BoardAction::Select(a.id.clone()));
                }
                if !compact {
                    ui.label(RichText::new(role_label(a.role)).color(theme.text_dim));
                }
                ui.label(RichText::new(work_state_label(a.state)).color(state_color(theme, a.state)));
            });
            if !compact && !a.provider.is_empty() {
                ui.label(RichText::new(&a.provider).color(theme.text_dim));
            }
            let line = if a.current_task.is_some() {
                format!(
                    "#{} {}",
                    a.current_task.unwrap_or(0),
                    if a.current_action.is_empty() {
                        a.current_task_title.clone()
                    } else {
                        plain(&a.current_action)
                    }
                )
            } else {
                String::new()
            };
            if !line.is_empty() {
                ui.label(RichText::new(ellipsis(&line, 40)).color(theme.text_dim))
                    .on_hover_text(line);
            }
            ui.horizontal_wrapped(|ui| {
                if a.assigned > 0 {
                    ui.label(
                        RichText::new(format!("{}/{}", a.done, a.assigned)).color(theme.text_dim),
                    );
                }
                ui.label(
                    RichText::new(view_model::elapsed_label(a.idle_secs)).color(theme.text_dim),
                );
            });
            for b in a.blockers.iter().take(2) {
                ui.label(RichText::new(ellipsis(&plain(b), 40)).color(theme.warn))
                    .on_hover_text(plain(b));
            }
            // ── 子エージェント行 ──
            let children: Vec<&TeamAgentView> = s
                .agents
                .iter()
                .filter(|c| c.parent_id.as_ref() == Some(&a.id))
                .collect();
            if children.is_empty() {
                return;
            }
            ui.separator();
            for c in children.iter().take(CHILD_ROWS_MAX) {
                child_row(ui, theme, c, acts);
            }
            let more = children.len().saturating_sub(CHILD_ROWS_MAX);
            if more > 0 {
                ui.label(
                    RichText::new(trf("team.child.more", &[("n", more.to_string())]))
                        .color(theme.text_dim),
                );
            }
        });
}

fn child_row(ui: &mut egui::Ui, theme: &Theme, c: &TeamAgentView, acts: &mut Vec<BoardAction>) {
    ui.horizontal(|ui| {
        glyph_label(ui, theme, c.state);
        let name = ui.selectable_label(false, RichText::new(ellipsis(&c.name, 14)).color(theme.text));
        if name.clicked() {
            acts.push(BoardAction::Select(c.id.clone()));
        }
        ui.label(RichText::new(work_state_label(c.state)).color(theme.text_dim));
        let action = plain(&c.current_action);
        if !action.is_empty() {
            ui.label(RichText::new(ellipsis(&action, 24)).color(theme.text_dim))
                .on_hover_text(action);
        }
        ui.label(RichText::new(view_model::elapsed_label(c.idle_secs)).color(theme.text_dim));
    });
}

// ── Tasks タブ ───────────────────────────────────────────────────────

/// Kanban の列。**状態そのものではなく、人が読む段**へ畳む。
///
/// **長さを人が書かない。** 固定長配列にすると、複数の枝が 1 列ずつ足した
/// ときに全員が同じ `N+1` を書き、git は衝突を出さないのに要素数が合わなく
/// なる (`keybinds::ALL_ACTIONS` で実際に起きた)。
const TASK_COLUMNS: &[(&str, &[TeamTaskState])] = &[
    (
        "team.col.waiting",
        &[TeamTaskState::Pending, TeamTaskState::Ready],
    ),
    (
        "team.col.working",
        &[
            TeamTaskState::Assigned,
            TeamTaskState::Running,
            TeamTaskState::RevisionRequired,
        ],
    ),
    (
        "team.col.checking",
        &[TeamTaskState::Validating, TeamTaskState::Reviewing],
    ),
    (
        "team.col.attention",
        &[
            TeamTaskState::Blocked,
            TeamTaskState::Failed,
            TeamTaskState::NeedsUser,
        ],
    ),
    ("team.col.done", &[TeamTaskState::Completed]),
];

fn tasks_tab(ui: &mut egui::Ui, theme: &Theme, s: &TeamSnapshot, acts: &mut Vec<BoardAction>) {
    if s.tasks.is_empty() {
        centered_note(ui, theme, &tr("team.empty.no_tasks"));
        return;
    }
    let w = (ui.available_width() / TASK_COLUMNS.len() as f32).max(140.0) - 8.0;
    egui::ScrollArea::horizontal()
        .id_salt(format!("team-scroll-{}", BoardTab::Tasks.key()))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                for (key, states) in TASK_COLUMNS.iter() {
                    let rows: Vec<&view_model::TaskView> =
                        s.tasks.iter().filter(|t| states.contains(&t.state)).collect();
                    ui.allocate_ui(egui::vec2(w, ui.available_height()), |ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new(format!("{} ({})", tr(key), rows.len()))
                                    .color(theme.text)
                                    .strong(),
                            );
                            ui.separator();
                            if rows.is_empty() {
                                return;
                            }
                            egui::ScrollArea::vertical()
                                .id_salt(format!("team-col-{key}"))
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    for t in rows {
                                        task_card(ui, theme, t, acts);
                                    }
                                });
                        });
                    });
                }
            });
        });
}

fn task_card(
    ui: &mut egui::Ui,
    theme: &Theme,
    t: &view_model::TaskView,
    acts: &mut Vec<BoardAction>,
) {
    egui::Frame::none()
        .fill(theme.panel)
        .stroke(egui::Stroke::new(1.0, theme.border))
        .inner_margin(egui::Margin::symmetric(6.0, 4.0))
        .rounding(3.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let title = format!("#{} {}", t.id, t.title);
            if ui
                .selectable_label(false, RichText::new(ellipsis(&title, 26)).color(theme.text))
                .on_hover_text(&title)
                .clicked()
            {
                acts.push(BoardAction::SelectTask(t.id));
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(task_state_label(t.state)).color(theme.text_dim));
                if let Some(a) = &t.assigned_agent {
                    ui.label(RichText::new(ellipsis(&a.0, 12)).color(theme.text_dim));
                }
                if t.attempts > 0 {
                    ui.label(
                        RichText::new(trf("team.task.attempts", &[("n", t.attempts.to_string())]))
                            .color(theme.warn),
                    );
                }
                if t.validation_ran {
                    ui.label(
                        RichText::new(if t.validation_ok { "✓" } else { "✗" })
                            .color(if t.validation_ok { theme.ok } else { theme.err }),
                    )
                    .on_hover_text(tr("team.task.validation"));
                }
                match t.review_verdict {
                    Some(ReviewVerdict::Approve) => {
                        ui.label(RichText::new("◎").color(theme.ok))
                            .on_hover_text(tr("team.review.approved"));
                    }
                    Some(ReviewVerdict::RequestChanges) => {
                        ui.label(RichText::new("✎").color(theme.warn))
                            .on_hover_text(tr("team.review.changes"));
                    }
                    None => {}
                }
            });
        });
    ui.add_space(3.0);
}

// ── Terminals タブ ───────────────────────────────────────────────────

fn terminals_tab(ui: &mut egui::Ui, theme: &Theme, s: &TeamSnapshot, acts: &mut Vec<BoardAction>) {
    let managed: Vec<&TeamAgentView> = s
        .agents
        .iter()
        .filter(|a| a.kind == AgentKind::ManagedSession)
        .collect();
    if managed.is_empty() {
        centered_note(ui, theme, &tr("team.empty.no_terminals"));
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt(format!("team-scroll-{}", BoardTab::Terminals.key()))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for a in managed {
                ui.horizontal(|ui| {
                    glyph_label(ui, theme, a.state);
                    ui.label(RichText::new(ellipsis(&a.name, 20)).color(theme.text));
                    ui.label(RichText::new(work_state_label(a.state)).color(theme.text_dim));
                    match a.session_id {
                        Some(sid) if a.can_open_terminal => {
                            if ui.button(tr("team.btn.open_terminal")).clicked() {
                                acts.push(BoardAction::OpenTerminal(sid));
                            }
                        }
                        _ => {
                            // **開けないボタンは無効にして理由を出す。**
                            ui.add_enabled(false, egui::Button::new(tr("team.btn.open_terminal")))
                                .on_disabled_hover_text(tr("team.terminal.not_started"));
                        }
                    }
                });
            }
        });
}

// ── Timeline タブ ────────────────────────────────────────────────────

fn timeline_tab(ui: &mut egui::Ui, theme: &Theme, s: &TeamSnapshot) {
    if s.events.is_empty() {
        centered_note(ui, theme, &tr("team.empty.no_events"));
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt(format!("team-scroll-{}", BoardTab::Timeline.key()))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for e in &s.events {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(clock(e.at)).color(theme.text_dim));
                    if let Some(a) = &e.actor {
                        ui.label(RichText::new(ellipsis(&a.0, 14)).color(theme.text_dim));
                    }
                    let text = plain(&e.summary);
                    ui.label(RichText::new(ellipsis(&text, 100)).color(theme.text))
                        .on_hover_text(text);
                });
            }
        });
}

/// epoch 秒 → `HH:MM` (UTC)。**依存クレートを増やさない最小実装。**
fn clock(secs: u64) -> String {
    format!("{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60)
}

// ── Mission Panel ────────────────────────────────────────────────────

fn mission_panel(ui: &mut egui::Ui, theme: &Theme, s: &TeamSnapshot, acts: &mut Vec<BoardAction>) {
    egui::ScrollArea::vertical()
        .id_salt("team-mission")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.label(RichText::new(tr("team.mission.phase")).color(theme.text).strong());
            for (i, (p, st)) in s.phases.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}.", i + 1)).color(theme.text_dim));
                    ui.label(RichText::new(phase_label(*p)).color(match st {
                        PhaseStatus::Done => theme.ok,
                        PhaseStatus::Running => theme.accent,
                        PhaseStatus::Waiting => theme.text_dim,
                    }));
                    ui.label(RichText::new(phase_status_label(*st)).color(theme.text_dim));
                });
            }

            // ── Human Decisions ── **0 件なら見出しごと出さない。**
            if !s.pending_decisions.is_empty() {
                ui.separator();
                ui.label(
                    RichText::new(tr("team.mission.decisions"))
                        .color(theme.warn)
                        .strong(),
                );
                for d in &s.pending_decisions {
                    egui::Frame::none()
                        .fill(theme.panel_alt)
                        .stroke(egui::Stroke::new(1.0, theme.warn))
                        .inner_margin(egui::Margin::symmetric(6.0, 4.0))
                        .rounding(3.0)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            let reason = plain(&d.reason);
                            ui.label(RichText::new(ellipsis(&reason, 60)).color(theme.text))
                                .on_hover_text(reason);
                            if !d.impact.is_empty() {
                                let imp = plain(&d.impact);
                                ui.label(RichText::new(ellipsis(&imp, 60)).color(theme.text_dim))
                                    .on_hover_text(imp);
                            }
                            ui.horizontal_wrapped(|ui| {
                                for opt in &d.options {
                                    let label = match opt.as_str() {
                                        "approve" => tr("team.decision.approve"),
                                        "reject" => tr("team.decision.reject"),
                                        "retry" => tr("team.decision.retry"),
                                        "reassign" => tr("team.decision.reassign"),
                                        _ => tr("team.decision.open_agent"),
                                    };
                                    if ui.button(label).clicked() {
                                        acts.push(match opt.as_str() {
                                            "approve" => BoardAction::Approve(d.id),
                                            "retry" => match d.task_id {
                                                Some(t) => BoardAction::Retry(t),
                                                None => BoardAction::Reject(d.id),
                                            },
                                            "reassign" => match d.task_id {
                                                Some(t) => BoardAction::Reassign(t),
                                                None => BoardAction::Reject(d.id),
                                            },
                                            _ => BoardAction::Reject(d.id),
                                        });
                                    }
                                }
                            });
                        });
                    ui.add_space(3.0);
                }
            }

            // ── Activity Feed ──
            if !s.events.is_empty() {
                ui.separator();
                ui.label(RichText::new(tr("team.mission.activity")).color(theme.text).strong());
                for e in s.events.iter().take(FEED_ROWS) {
                    let text = plain(&e.summary);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(clock(e.at)).color(theme.text_dim));
                        ui.label(RichText::new(ellipsis(&text, 44)).color(theme.text_dim))
                            .on_hover_text(text);
                    });
                }
            }
        });
}

// ── Current Action Bar ───────────────────────────────────────────────

fn current_action_bar(
    ui: &mut egui::Ui,
    theme: &Theme,
    s: &TeamSnapshot,
    acts: &mut Vec<BoardAction>,
) {
    let a = current_action(s);
    let col = if a.urgent { theme.warn } else { theme.text };
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(a.glyph).color(col));
        let text = plain(&a.text);
        let resp = ui.selectable_label(false, RichText::new(ellipsis(&text, 110)).color(col));
        if resp.on_hover_text(&text).clicked() {
            match &a.focus {
                ActionFocus::Decision(_) => {}
                ActionFocus::Agent(id) => acts.push(BoardAction::Select(id.clone())),
                ActionFocus::Task(t) => acts.push(BoardAction::SelectTask(*t)),
                ActionFocus::None => {}
            }
        }
    });
}

// ── 空状態と復元 ─────────────────────────────────────────────────────

/// **空状態は利用可能領域の中央に 1 枚のカードで出す** (下や上に取り残さない)。
fn centered_note(ui: &mut egui::Ui, theme: &Theme, text: &str) {
    let avail = ui.available_size();
    ui.allocate_ui_with_layout(
        avail,
        egui::Layout::centered_and_justified(egui::Direction::TopDown),
        |ui| {
            ui.label(RichText::new(text).color(theme.text_dim));
        },
    );
}

fn empty_card(ui: &mut egui::Ui, theme: &Theme, acts: &mut Vec<BoardAction>) {
    let avail = ui.available_size();
    ui.allocate_ui_with_layout(
        avail,
        egui::Layout::centered_and_justified(egui::Direction::TopDown),
        |ui| {
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(tr("team.empty.title")).color(theme.text).strong());
                ui.label(RichText::new(tr("team.empty.hint")).color(theme.text_dim));
                ui.add_space(6.0);
                if ui.button(tr("team.btn.new_run")).clicked() {
                    acts.push(BoardAction::OpenNewRun);
                }
            });
        },
    );
}

fn restore_card(
    ui: &mut egui::Ui,
    theme: &Theme,
    restore: RestorePrompt,
    acts: &mut Vec<BoardAction>,
) {
    let avail = ui.available_size();
    ui.allocate_ui_with_layout(
        avail,
        egui::Layout::centered_and_justified(egui::Direction::TopDown),
        |ui| {
            ui.vertical_centered(|ui| {
                if restore == RestorePrompt::ConfirmDiscard {
                    ui.label(
                        RichText::new(tr("team.restore.confirm_discard"))
                            .color(theme.err)
                            .strong(),
                    );
                    ui.horizontal(|ui| {
                        if ui.button(tr("team.restore.discard")).clicked() {
                            acts.push(BoardAction::DiscardRun);
                        }
                        if ui.button(tr("team.restore.cancel")).clicked() {
                            acts.push(BoardAction::ResumeRun);
                        }
                    });
                    return;
                }
                ui.label(
                    RichText::new(tr("team.restore.found"))
                        .color(theme.text)
                        .strong(),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(tr("team.restore.resume")).clicked() {
                        acts.push(BoardAction::ResumeRun);
                    }
                    if ui.button(tr("team.restore.read_only")).clicked() {
                        acts.push(BoardAction::OpenReadOnly);
                    }
                    if ui.button(tr("team.restore.discard")).clicked() {
                        acts.push(BoardAction::DiscardRun);
                    }
                });
            });
        },
    );
}

// ── New Team Run フォーム ────────────────────────────────────────────

fn new_run_form(
    ui: &mut egui::Ui,
    theme: &Theme,
    form: &mut NewRunForm,
    acts: &mut Vec<BoardAction>,
) {
    ui.label(RichText::new(tr("team.form.title")).color(theme.text).strong());
    ui.separator();
    egui::Grid::new("team-new-run")
        .num_columns(2)
        .spacing([10.0, 6.0])
        .show(ui, |ui| {
            ui.label(tr("team.form.goal_name"));
            ui.text_edit_singleline(&mut form.goal_name);
            ui.end_row();

            ui.label(tr("team.form.spec_source"));
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(form.from_file, tr("team.form.from_file"))
                    .clicked()
                {
                    form.from_file = true;
                }
                if ui
                    .selectable_label(!form.from_file, tr("team.form.direct"))
                    .clicked()
                {
                    form.from_file = false;
                }
            });
            ui.end_row();

            if form.from_file {
                ui.label(tr("team.form.spec_path"));
                ui.text_edit_singleline(&mut form.spec_path);
                ui.end_row();
            } else {
                ui.label(tr("team.form.spec_text"));
                ui.add(
                    egui::TextEdit::multiline(&mut form.spec_text)
                        .desired_rows(8)
                        .desired_width(ui.available_width()),
                );
                ui.end_row();
            }

            ui.label(tr("team.form.agents"));
            ui.add(egui::Slider::new(&mut form.agents, 1..=16));
            ui.end_row();

            ui.label(tr("team.form.max_attempts"));
            ui.add(egui::Slider::new(&mut form.max_attempts, 1..=5));
            ui.end_row();

            ui.label(tr("team.form.approval_mode"));
            ui.horizontal(|ui| {
                for (id, key) in [
                    ("ask", "team.form.approval_ask"),
                    ("auto", "team.form.approval_auto"),
                    ("agent", "team.form.approval_agent"),
                ] {
                    if ui
                        .selectable_label(form.approval_mode == id, tr(key))
                        .clicked()
                    {
                        form.approval_mode = id.to_string();
                    }
                }
            });
            ui.end_row();

            ui.label(tr("team.form.review_required"));
            ui.checkbox(&mut form.review_required, tr("team.form.review_hint"));
            ui.end_row();

            ui.label(tr("team.form.roles"));
            ui.horizontal_wrapped(|ui| {
                for r in TeamRole::ALL {
                    // TeamLead は必ず 1 体居るので選択肢に出さない
                    // (選べない選択肢を並べない)。
                    if r == TeamRole::TeamLead {
                        continue;
                    }
                    let on = form.roles.contains(&r);
                    if ui.selectable_label(on, role_label(r)).clicked() {
                        if on {
                            form.roles.retain(|x| *x != r);
                        } else {
                            form.roles.push(r);
                        }
                    }
                }
            });
            ui.end_row();

            ui.label(tr("team.form.cost_limit"));
            ui.add(egui::DragValue::new(&mut form.cost_limit).range(0.0..=1000.0));
            ui.end_row();
        });
    if !form.error.is_empty() {
        ui.colored_label(theme.err, plain(&form.error));
    }
    ui.separator();
    ui.horizontal(|ui| {
        if ui.button(tr("team.form.plan_preview")).clicked() {
            acts.push(BoardAction::PlanFromForm);
        }
        if ui.button(tr("team.form.cancel")).clicked() {
            form.open = false;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 省略は元より長くならない() {
        assert_eq!(ellipsis("abc", 5), "abc");
        assert_eq!(ellipsis("abcdef", 4), "abc…");
        assert_eq!(ellipsis("あいうえお", 3), "あい…");
    }

    #[test]
    fn 出力を1行へ畳む() {
        assert_eq!(plain("a\nb\tc  d"), "a b c d");
        // **制御文字は空白へ潰すだけで、意味は解釈しない。**
        // 画面から取るテキストは vt100 が解いた後なので escape は残らないが、
        // 万一混ざっても「egui のマークアップとして解釈しない」を守る。
        assert_eq!(plain("\u{1b}[31mred\u{1b}[0m"), "[31mred [0m");
        assert!(!plain("<b>x</b>").is_empty());
    }

    #[test]
    fn かんばんの列は全状態を漏れなく一度だけ拾う() {
        let mut seen = std::collections::BTreeSet::new();
        for (_, states) in TASK_COLUMNS {
            for s in states.iter() {
                assert!(seen.insert(*s), "{} が 2 つの列に出る", s.key());
            }
        }
        for s in TeamTaskState::ALL {
            assert!(seen.contains(&s), "{} がどの列にも出ない", s.key());
        }
    }

    #[test]
    fn 時刻表記は00から23時のあいだ() {
        assert_eq!(clock(0), "00:00");
        assert_eq!(clock(3661), "01:01");
        assert_eq!(clock(86_399), "23:59");
    }

    #[test]
    fn 子エージェント行の上限がある() {
        // 64 体でも縦に伸び続けないための線。
        assert!(CHILD_ROWS_MAX <= 10);
        assert!(FEED_ROWS <= 20);
    }
}
