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
use super::panel::{BoardAction, BoardTab, DraftState, NewRunForm, RestorePrompt};
use super::planner;
use super::view_model::{
    self, current_action, ActionFocus, TeamAgentView, TeamSnapshot, MISSION_PANEL_W,
};

/// 子エージェント行を 1 枚のカードに出す上限。**超えたぶんは「他 N 件」**
/// (64 体でも縦に伸び続けないための線)。
pub const CHILD_ROWS_MAX: usize = 6;
/// 窓と画面のあいだに残す余白 (片側)。**0 にすると角が画面の縁に張り付く。**
const WINDOW_MARGIN: f32 = 16.0;
/// 窓の枠 (内側余白とタイトルバー) が外形に足すぶん。
///
/// **`max_width` / `max_height` が縛るのは中身**なので、枠を引かずに
/// 「画面 − 余白」を渡すと外形はそのぶんだけ画面をはみ出す。値は実測
/// (横 12px / 縦 44px) に余裕を足したもので、**正しさを保つのは定数ではなく
/// `team画面はどの画面幅でも画面に収まる`** — テーマや egui の余白が変われば
/// そちらが落ちる。
const WINDOW_CHROME_W: f32 = 16.0;
const WINDOW_CHROME_H: f32 = 48.0;

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


/// 状態を色つきの小さな札で出す。
///
/// **色は補助**で、意味は記号 ([`AgentWorkState::glyph`]) と文字が持つ。
/// カードの中で「記号・文字・色」が 3 か所に散っていると、状態を読むのに
/// 目が 3 回動く。1 つの札にまとめると 1 回で済む。
fn state_chip(ui: &mut egui::Ui, theme: &Theme, s: AgentWorkState) {
    let col = state_color(theme, s);
    egui::Frame::none()
        .fill(col.linear_multiply(0.16))
        .stroke(egui::Stroke::new(1.0_f32, col.linear_multiply(0.55)))
        .inner_margin(egui::Margin::symmetric(5.0, 1.0))
        .rounding(7.0)
        .show(ui, |ui| {
            // 札の中は横並び (`Frame` は親のレイアウトを継承するので明示する)。
            ui.horizontal(|ui| {
                glyph_label(ui, theme, s);
                ui.label(RichText::new(work_state_label(s)).color(col));
            });
        });
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
    // 端末タブで「その場で開いている」担当。
    expanded: Option<&AgentId>,
    // 走っている Run の一覧 `(表題, 進行中か)` と、いま出している位置。
    runs: &[(String, bool)],
    active_run: usize,
    notice: &str,
    // 端末 1 枚を「ここへ」描く口。描けたら true。
    // **実体は app 側にある** (セッションを持っているのはあちら) ので、
    // 盤面は場所だけ用意して呼ぶ。
    term: &mut dyn FnMut(&mut egui::Ui, SessionId) -> bool,
) -> Vec<BoardAction> {
    let mut acts = Vec::new();
    if !open {
        return acts;
    }
    let mut win_open = true;
    // **画面より広い窓を作らない。** 中身 (レーン + Mission Panel) が
    // 既定幅を超えると `resizable` な窓は伸びる。中央寄せなので伸びたぶんは
    // **左右へ均等にはみ出し**、見出しが両端で切れる (実際にそうなっていた)。
    // 上限を画面から取れば、あふれるぶんは横スクロールへ逃げる
    // (CLAUDE.md「どの幅でも見切れない」)。
    let screen = ctx.screen_rect();
    let max_w = (screen.width() - WINDOW_MARGIN * 2.0 - WINDOW_CHROME_W).max(160.0);
    let max_h = (screen.height() - WINDOW_MARGIN * 2.0 - WINDOW_CHROME_H).max(160.0);
    egui::Window::new(tr("team.window_title"))
        .open(&mut win_open)
        .collapsible(false)
        .resizable(true)
        // **最初から画面いっぱいで開く。**
        //
        // 既定を 1180x760 にしていたので、レーンや担当が増えるたびに窓が
        // 少しずつ広がっていた (「徐々に画面が広がる」として報告された)。
        // 上限 (`max_w` / `max_h`) と同じ値から始めれば、中身が増えても
        // それ以上には広がらない = 開いた瞬間の大きさのまま落ち着く。
        // `resizable` は残すので、狭めたい人は狭められる。
        .default_width(max_w)
        .default_height(max_h)
        .max_width(max_w)
        .max_height(max_h)
        .constrain_to(screen)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            body(
                ui, theme, snap, tab, form, restore, selected, expanded, runs, active_run,
                notice, &mut acts, term,
            );
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
    // 端末タブで「その場で開いている」担当。
    expanded: Option<&AgentId>,
    // 走っている Run の一覧 `(表題, 進行中か)` と、いま出している位置。
    runs: &[(String, bool)],
    active_run: usize,
    notice: &str,
    acts: &mut Vec<BoardAction>,
    term: &mut dyn FnMut(&mut egui::Ui, SessionId) -> bool,
) {
    if !notice.is_empty() {
        ui.colored_label(theme.warn, plain(notice));
    }
    run_tabs_row(ui, theme, runs, active_run, acts);

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
        // **`allocate_ui` は親のレイアウトを継承する。**
        // `egui-0.29.1/src/ui.rs:1340` が `self.allocate_ui_with_layout(.., *self.layout(), ..)`
        // なので、`horizontal_top` の中で呼ぶと**中の子も左→右に並ぶ**。
        // これで組織図は「Team Lead カードとレーン群が横に並ぶ」、Mission Panel は
        // 「開発フェーズが 1 行に伸びて画面外へ見切れる」という崩れ方をしていた。
        // 縦積みは明示する (継承させない)。
        let column = egui::Layout::top_down(egui::Align::Min);
        ui.allocate_ui_with_layout(egui::vec2(board_w, content_h), column, |ui| match tab {
            BoardTab::Organization => organization_tab(ui, theme, s, &layout, selected, acts),
            BoardTab::Tasks => tasks_tab(ui, theme, s, acts),
            BoardTab::Terminals => terminals_tab(ui, theme, s, expanded, acts, term),
            BoardTab::Timeline => timeline_tab(ui, theme, s),
        });
        if layout.mission_panel {
            ui.separator();
            ui.allocate_ui_with_layout(
                egui::vec2(MISSION_PANEL_W - 12.0, content_h),
                column,
                |ui| {
                    mission_panel(ui, theme, s, acts);
                },
            );
        }
    });

    ui.separator();
    current_action_bar(ui, theme, s, acts);
}

// ── Top Command Bar ──────────────────────────────────────────────────

fn top_command_bar(
    ui: &mut egui::Ui,
    theme: &Theme,
    s: &TeamSnapshot,
    acts: &mut Vec<BoardAction>,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(ellipsis(&s.goal.title, 44))
                .color(theme.text)
                .strong(),
        )
        .on_hover_text(&s.goal.title);
        ui.label(
            RichText::new(format!("[{}]", goal_status_label(s.goal.status)))
                .color(goal_color(theme, s.goal.status)),
        );
        ui.label(RichText::new(phase_label(s.goal.phase)).color(theme.text_dim));
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
        ui.label(trf(
            "team.metrics.agents",
            &[("n", m.agents_active.to_string())],
        ));
        // **自動検証が 1 本も無いことを、常に見えるところへ出す。**
        // 通知は上書きで消えるが、これは状態から導くので消えない。
        // 完了を決めるのがレビュー承認だけになる、という重い意味を持つ。
        if s.unvalidated {
            ui.label(
                RichText::new(tr("team.chip.unvalidated"))
                    .color(theme.warn)
                    .strong(),
            )
            .on_hover_text(tr("team.chip.unvalidated_hint"));
        }
        // **常に 0 のバッジを出さない** (CLAUDE.md: 減らせないかを先に考える)。
        if m.blocked > 0 {
            ui.label(
                RichText::new(trf("team.metrics.blocked", &[("n", m.blocked.to_string())]))
                    .color(theme.warn),
            );
        }
        if m.tests_passed > 0 {
            ui.label(
                RichText::new(trf(
                    "team.metrics.tests",
                    &[("n", m.tests_passed.to_string())],
                ))
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
        // **途中で口を出す入口。** 相手は開いた先の一覧から選ぶので、
        // 盤面のカードを探し当てる必要が無い。端末を持つ相手が 1 体も
        // 居ないときは、押せるのに何も起きないボタンにしない。
        let live = s.agents.iter().any(|a| a.can_open_terminal);
        let b = ui.add_enabled(live, egui::Button::new(tr("team.btn.instruct")));
        let b = if live {
            b.on_hover_text(tr("team.btn.instruct_hint"))
        } else {
            b.on_disabled_hover_text(tr("team.terminal.not_started"))
        };
        if b.clicked() {
            acts.push(BoardAction::OpenInstruct);
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
                // 縦積みを明示する (`allocate_ui` は親の横並びを継承してしまう)。
                ui.allocate_ui_with_layout(
                    egui::vec2(layout.lane_w - 8.0, ui.available_height()),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        lane_ui(ui, theme, s, lane, selected, layout.compact, acts);
                    },
                );
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
        .stroke(egui::Stroke::new(1.0_f32, theme.border))
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
                ui.label(trf("team.lead.teams", &[("n", s.teams.len().to_string())]));
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
                    RichText::new(view_model::elapsed_label(lead.idle_secs)).color(theme.text_dim),
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
            ui.label(
                RichText::new(ellipsis(&lane.name, 20))
                    .color(theme.text)
                    .strong(),
            )
            .on_hover_text(&lane.name);
            // **常に 0 のバッジを出さない。**
            if lane.total > 0 {
                ui.label(
                    RichText::new(format!("{}/{}", lane.done, lane.total)).color(theme.text_dim),
                );
            }
        });
        // **進み具合は数字より先に「形」で分かるようにする。**
        // 仕事が 1 つも無い列にバーを出すと、無いものを「0%」と言うことに
        // なるので出さない (区切り線で代える)。アニメーションはしない
        // (設計原則 3: アイドルのコストはゼロ)。
        if lane.total > 0 {
            ui.add(
                egui::ProgressBar::new(lane.done as f32 / lane.total as f32)
                    .desired_height(3.0)
                    .rounding(2.0)
                    .fill(theme.accent),
            );
            ui.add_space(3.0);
        } else {
            ui.separator();
        }
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
            1.0_f32,
            if is_sel { theme.accent } else { theme.border },
        ))
        .inner_margin(egui::Margin::symmetric(8.0, 6.0))
        .rounding(4.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                if ui
                    .selectable_label(
                        is_sel,
                        RichText::new(ellipsis(&a.name, 18)).color(theme.text).strong(),
                    )
                    .on_hover_text(tr("team.card.click_to_instruct"))
                    .clicked()
                {
                    acts.push(BoardAction::Select(a.id.clone()));
                }
                state_chip(ui, theme, a.state);
                if !compact {
                    ui.label(RichText::new(role_label(a.role)).color(theme.text_dim));
                }
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
        let name = ui.selectable_label(
            false,
            RichText::new(ellipsis(&c.name, 14)).color(theme.text),
        );
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
                    let rows: Vec<&view_model::TaskView> = s
                        .tasks
                        .iter()
                        .filter(|t| states.contains(&t.state))
                        .collect();
                    // 縦積みを明示する (`allocate_ui` は親の横並びを継承してしまう)。
                    ui.allocate_ui_with_layout(
                        egui::vec2(w, ui.available_height()),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
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
                        },
                    );
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
        .stroke(egui::Stroke::new(1.0_f32, theme.border))
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

/// 札の中に本物の端末を描くときの高さ。
///
/// **画面を占領しない大きさにする。** 高くすると他の担当が視界から
/// 消えるので、「チームを見ながら 1 人を覗く」ができなくなる。
const TERMINAL_CARD_H: f32 = 320.0;

// ── Terminals タブ ───────────────────────────────────────────────────

/// **端末タブ = デッキ。** 名前とボタンだけでは「何をしているか」が分からない。
///
/// 以前ここは一覧とボタンだけで、走っている最中に中身を見るには端末を開く
/// しか無かった (開くと中央ビューが切り替わるので、「ちょっと様子を見たい」
/// に対して代償が大きい)。デッキと同じように、**1 体 1 枚の札**に直近の
/// 出力を出す。
///
/// 幅は `ui.available_width()` に必ず収める (どの幅でも見切れない)。
fn terminals_tab(
    ui: &mut egui::Ui,
    theme: &Theme,
    s: &TeamSnapshot,
    expanded: Option<&AgentId>,
    acts: &mut Vec<BoardAction>,
    term: &mut dyn FnMut(&mut egui::Ui, SessionId) -> bool,
) {
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
            let w = ui.available_width();
            for a in managed {
                let open = expanded == Some(&a.id);
                agent_deck_card(ui, theme, a, w, open, acts, term);
                ui.add_space(6.0);
            }
        });
}

/// 端末タブの札 1 枚。
fn agent_deck_card(
    ui: &mut egui::Ui,
    theme: &Theme,
    a: &TeamAgentView,
    width: f32,
    open: bool,
    acts: &mut Vec<BoardAction>,
    term: &mut dyn FnMut(&mut egui::Ui, SessionId) -> bool,
) {
    egui::Frame::none()
        .fill(theme.panel)
        .stroke(egui::Stroke::new(1.0_f32, theme.border))
        .inner_margin(egui::Margin::same(8.0))
        .rounding(8.0)
        .show(ui, |ui| {
            ui.set_width((width - 16.0).max(120.0));
            ui.horizontal(|ui| {
                state_chip(ui, theme, a.state);
                ui.label(RichText::new(ellipsis(&a.name, 24)).color(theme.text));
                ui.label(RichText::new(role_label(a.role)).color(theme.text_dim));
                // ボタンは右端へ寄せる (狭いときも本文を押し出さない)。
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match a.session_id {
                        Some(sid) if a.can_open_terminal => {
                            // **切り替えずに見る**のが既定。画面が入れ替わると
                            // 「ちょっと様子を見たい」に対して代償が大きい。
                            let label = if open {
                                tr("team.btn.fold_output")
                            } else {
                                tr("team.btn.show_output")
                            };
                            if ui.button(label).clicked() {
                                acts.push(BoardAction::ToggleAgentOutput(a.id.clone()));
                            }
                            if ui
                                .button(tr("team.btn.open_terminal"))
                                .on_hover_text(tr("team.btn.open_terminal_hint"))
                                .clicked()
                            {
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
            });
            // いま何のタスクを持っているか (無ければ行ごと出さない)。
            if !a.current_task_title.is_empty() {
                let t = plain(&a.current_task_title);
                ui.label(RichText::new(ellipsis(&t, 90)).color(theme.text_dim))
                    .on_hover_text(t);
            }
            // 直近の出力。**空なら枠ごと出さない** (空白を作らない)。
            let body = plain(&a.preview);
            if body.trim().is_empty() {
                return;
            }
            ui.add_space(4.0);
            // **開いたら本物の端末をここへ描く。**
            //
            // 画面を切り替えて裏で見るのではなく、チームの盤面の中で
            // そのまま見えること — 「端末を開く」で中央ビューが入れ替わると、
            // 「ちょっと様子を見たい」に対して代償が大きすぎる。
            if open {
                if let Some(sid) = a.session_id {
                    let mut drawn = false;
                    egui::Frame::none()
                        .fill(theme.bg)
                        .rounding(6.0)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.set_height(TERMINAL_CARD_H);
                            drawn = term(ui, sid);
                        });
                    if drawn {
                        return;
                    }
                }
            }
            let all: Vec<&str> = body.lines().collect();
            // 畳んでいるときは末尾だけ (札が縦に伸びて一覧にならなくなる)。
            let from = if open {
                0
            } else {
                all.len().saturating_sub(view_model::PREVIEW_LINES_FOLDED)
            };
            egui::Frame::none()
                .fill(theme.bg)
                .inner_margin(egui::Margin::same(6.0))
                .rounding(6.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let draw = |ui: &mut egui::Ui| {
                        for line in &all[from..] {
                            ui.label(
                                RichText::new(ellipsis(line, 160))
                                    .monospace()
                                    .size(10.5)
                                    .color(theme.text_dim),
                            );
                        }
                    };
                    if open {
                        // **開いても画面を占領しない。** 上限を超えたぶんは
                        // この枠の中でスクロールする (他の担当が見えなくならない)。
                        egui::ScrollArea::vertical()
                            .id_salt(format!("team-out-{}", a.id.0))
                            .max_height(360.0)
                            .stick_to_bottom(true)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                draw(ui);
                            });
                    } else {
                        draw(ui);
                    }
                });
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
            ui.label(
                RichText::new(tr("team.mission.phase"))
                    .color(theme.text)
                    .strong(),
            );
            // **段は上から下へ 1 行ずつ。** 記号で状態が読めるので、
            // 「完了 / 待ち」の文字は重ねて出さない (走っている段だけ、
            // どこに居るかが要るので添える)。**色だけに頼らない**。
            for (i, (p, st)) in s.phases.iter().enumerate() {
                let col = match st {
                    PhaseStatus::Done => theme.ok,
                    PhaseStatus::Running => theme.accent,
                    PhaseStatus::Waiting => theme.text_dim,
                };
                let mark = match st {
                    PhaseStatus::Done => "✓",
                    PhaseStatus::Running => "▶",
                    PhaseStatus::Waiting => "・",
                };
                ui.horizontal(|ui| {
                    ui.label(RichText::new(mark).color(col));
                    let name = format!("{}. {}", i + 1, phase_label(*p));
                    let name = if *st == PhaseStatus::Running {
                        RichText::new(name).color(col).strong()
                    } else {
                        RichText::new(name).color(col)
                    };
                    ui.label(name).on_hover_text(phase_status_label(*st));
                    if *st == PhaseStatus::Running {
                        ui.label(
                            RichText::new(phase_status_label(*st)).color(theme.text_dim),
                        );
                    }
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
                        .stroke(egui::Stroke::new(1.0_f32, theme.warn))
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
                ui.label(
                    RichText::new(tr("team.mission.activity"))
                        .color(theme.text)
                        .strong(),
                );
                for e in s.events.iter().take(FEED_ROWS) {
                    let text = plain(&e.summary);
                    // **文字数で切らない。** 「何文字なら入るか」は日本語と
                    // 英語で違ううえ、パネル幅にも依る (44 文字で切っても
                    // 右端で 1 文字ぶん欠けていた)。**残り幅で切る**のは
                    // egui にやらせる。全文はホバーで出す。
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(clock(e.at)).color(theme.text_dim));
                        ui.add(
                            egui::Label::new(RichText::new(&text).color(theme.text_dim))
                                .truncate(),
                        )
                        .on_hover_text(&text);
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
        // **押しても何も起きないものを押せる見た目にしない。**
        let clickable = match &a.focus {
            ActionFocus::None => false,
            ActionFocus::Decision(id) => s
                .pending_decisions
                .iter()
                .any(|d| d.id == *id && d.task_id.is_some()),
            _ => true,
        };
        let label = RichText::new(ellipsis(&text, 110)).color(col);
        let resp = if clickable {
            ui.selectable_label(false, label)
        } else {
            ui.label(label)
        };
        if clickable && resp.on_hover_text(&text).clicked() {
            match &a.focus {
                // 判断はタスクに紐づくので、そのタスクを開く。紐づかない
                // ものは Mission Panel にしか出ないので、何もしない代わりに
                // **押せる見た目にしない** (下の `clickable`)。
                ActionFocus::Decision(id) => {
                    if let Some(t) = s
                        .pending_decisions
                        .iter()
                        .find(|d| d.id == *id)
                        .and_then(|d| d.task_id)
                    {
                        acts.push(BoardAction::SelectTask(t));
                    }
                }
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
                ui.label(
                    RichText::new(tr("team.empty.title"))
                        .color(theme.text)
                        .strong(),
                );
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

/// **同時に走っている Run の切り替え。**
///
/// 1 本しか無いときは**出さない** (常に 1 つしか無い選択肢は、
/// 場所を取るだけで何も選ばせない)。
fn run_tabs_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    runs: &[(String, bool)],
    active: usize,
    acts: &mut Vec<BoardAction>,
) {
    if runs.len() < 2 {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        for (i, (title, running)) in runs.iter().enumerate() {
            // 進行中かどうかは記号で示す (色は補助)。
            let mark = if *running { "▶" } else { "✓" };
            let label = format!("{mark} {}", ellipsis(title, 20));
            let r = ui.selectable_label(i == active, RichText::new(label).color(theme.text));
            if r.clicked() {
                acts.push(BoardAction::SelectRun(i));
            }
            // 閉じるのは**いま出している 1 本だけ**に出す。全部に出すと
            // 押し間違いで別のチームを止めてしまう。
            if i == active
                && ui
                    .small_button("✕")
                    .on_hover_text(tr("team.btn.close_run"))
                    .clicked()
            {
                acts.push(BoardAction::CloseRun(i));
            }
        }
    });
    ui.separator();
}

// ── New Team Run フォーム ────────────────────────────────────────────

fn new_run_form(
    ui: &mut egui::Ui,
    theme: &Theme,
    form: &mut NewRunForm,
    acts: &mut Vec<BoardAction>,
) {
    ui.label(
        RichText::new(tr("team.form.title"))
            .color(theme.text)
            .strong(),
    );
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
                // **`tr` には素の文字列リテラルを渡す。** 変数越しに渡すと
                // `zai i18n missing` の走査に 1 度も現れず、辞書から抜けても
                // 誰も気付かないまま**全言語で ID がそのまま画面に出る**
                // (実際にこの 3 つがその状態だった)。
                for (id, label) in [
                    ("ask", tr("team.form.approval_ask")),
                    ("auto", tr("team.form.approval_auto")),
                    ("agent", tr("team.form.approval_agent")),
                ] {
                    if ui
                        .selectable_label(form.approval_mode == id, label)
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
    spec_draft_section(ui, theme, form, acts);
    ui.separator();
    ui.horizontal(|ui| {
        // 下書きの確認中は、計画のボタンを出さない。
        // **同じ瞬間に 2 つの進み方を見せない** (「これでいいですか？」に
        // 答える前に計画へ進めると、確認の意味が無くなる)。
        if matches!(form.draft, DraftState::Ready { .. }) {
            if ui.button(tr("team.form.cancel")).clicked() {
                form.open = false;
            }
            return;
        }
        if ui.button(tr("team.form.plan_preview")).clicked() {
            acts.push(BoardAction::PlanFromForm);
        }
        if ui.button(tr("team.form.cancel")).clicked() {
            form.open = false;
        }
    });
}

/// **短い指示を仕様書へ書き換える段。**
///
/// 出るのは「このままでは実装タスクが 1 件にしかならない」ときだけ
/// ([`planner::needs_spec_rewrite`])。分かれる SPEC を書いた人に
/// 余計な段を見せない。
fn spec_draft_section(
    ui: &mut egui::Ui,
    theme: &Theme,
    form: &NewRunForm,
    acts: &mut Vec<BoardAction>,
) {
    match &form.draft {
        DraftState::Idle => {
            // ファイル指定のときは中身を読まないと判定できないので、
            // 直接入力の本文だけを見る (読み込みは押されてから)。
            let short = form.from_file || planner::needs_spec_rewrite(&form.spec_text);
            if !short {
                return;
            }
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                if ui.button(tr("team.draft.button")).clicked() {
                    acts.push(BoardAction::DraftSpec);
                }
                ui.label(RichText::new(tr("team.draft.hint")).color(theme.text_dim));
            });
        }
        DraftState::Running { agent } => {
            ui.separator();
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    RichText::new(trf("team.draft.running", &[("agent", agent.clone())]))
                        .color(theme.text_dim),
                );
            });
        }
        DraftState::Ready { agent, text } => {
            ui.separator();
            ui.label(
                RichText::new(trf("team.draft.question", &[("agent", agent.clone())]))
                    .color(theme.text)
                    .strong(),
            );
            // **中身を全部見せてから決めてもらう。** 折り畳んだまま
            // 「はい」を押させると、確認したことにならない。
            egui::ScrollArea::vertical()
                .id_salt("team-draft-preview")
                .max_height(260.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for line in text.lines() {
                        ui.label(RichText::new(line).monospace().size(11.0).color(theme.text));
                    }
                });
            ui.horizontal_wrapped(|ui| {
                if ui.button(tr("team.draft.accept")).clicked() {
                    acts.push(BoardAction::AcceptDraft);
                }
                if ui.button(tr("team.draft.retry")).clicked() {
                    acts.push(BoardAction::DraftSpec);
                }
                if ui.button(tr("team.draft.keep")).clicked() {
                    acts.push(BoardAction::DiscardDraft);
                }
            });
        }
        DraftState::Failed { why } => {
            ui.separator();
            ui.colored_label(theme.warn, plain(why));
            ui.horizontal_wrapped(|ui| {
                if ui.button(tr("team.draft.retry")).clicked() {
                    acts.push(BoardAction::DraftSpec);
                }
                if ui.button(tr("team.draft.keep")).clicked() {
                    acts.push(BoardAction::DiscardDraft);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **どの画面幅でも、Team 画面が画面の外へはみ出さない。**
    ///
    /// 中身 (レーン + Mission Panel) が既定幅を超えると `resizable` な窓は
    /// 伸びる。中央寄せなので伸びたぶんは**左右へ均等にはみ出し**、
    /// 見出しが両端で切れる (実際にそうなっていた: 「Implementation」が
    /// 「mplementation」に、Mission Panel の行が右端で欠けていた)。
    ///
    /// **実 egui で 1 枚描いて、描かれた矩形が画面に収まることを見る。**
    #[test]
    fn team画面はどの画面幅でも画面に収まる() {
        let rt = super::super::runtime_tests::started(4);
        let snap = view_model::snapshot(&rt, 100);
        let theme = crate::theme::all().remove(0);
        // 極端な形も混ぜる (CLAUDE.md: 900×700 / 1200×300 のような比)。
        for (w, h) in [
            (900.0_f32, 700.0_f32),
            (1200.0, 300.0),
            (1720.0, 1148.0),
            (700.0, 480.0),
        ] {
            let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h));
            let ctx = egui::Context::default();
            let input = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            let mut form = NewRunForm::default();
            let mut used = egui::Rect::NOTHING;
            // **数フレーム回す。** 窓の大きさは 1 フレーム目には決まらない。
            for _ in 0..4 {
                let _ = ctx.run(input.clone(), |ctx| {
                    board_window(
                        ctx,
                        &theme,
                        true,
                        Some(&snap),
                        BoardTab::Organization,
                        &mut form,
                        RestorePrompt::None,
                        None,
                        None,
                        &[],
                        0,
                        "",
                        // 端末は描かない (ヘッドレスにセッションは無い)。
                        // **描けなかったときの経路**もここで踏まれる。
                        &mut |_ui, _sid| false,
                    );
                });
                used = ctx.used_rect();
            }
            assert!(
                used.left() >= screen.left() - 1.0 && used.right() <= screen.right() + 1.0,
                "画面 {w}×{h} で横にはみ出した: used={used:?} screen={screen:?}"
            );
            assert!(
                used.top() >= screen.top() - 1.0 && used.bottom() <= screen.bottom() + 1.0,
                "画面 {w}×{h} で縦にはみ出した: used={used:?} screen={screen:?}"
            );
        }
    }

    /// **`allocate_ui` は親のレイアウトを継承する** (egui 0.29)。
    ///
    /// これが Team 画面の崩れの正体だった。`egui-0.29.1/src/ui.rs:1340` が
    /// `self.allocate_ui_with_layout(.., *self.layout(), ..)` なので、
    /// `horizontal_top` の中で呼ぶと**中の子まで左→右に並ぶ**。実害は
    /// 「Team Lead カードとレーン群が横に並ぶ」「開発フェーズが 1 行に
    /// 伸びて画面外へ見切れる」の 2 つ。
    ///
    /// **不具合そのものを実 egui で再現して比べる。** egui 側が振る舞いを
    /// 変えたら、前半が落ちて気付ける。
    #[test]
    fn 横並びの中では縦積みを明示しないと横に並ぶ() {
        fn two_labels(explicit: bool) -> (egui::Rect, egui::Rect) {
            let ctx = egui::Context::default();
            let (mut a, mut b) = (egui::Rect::NOTHING, egui::Rect::NOTHING);
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal_top(|ui| {
                        let size = egui::vec2(220.0, 120.0);
                        let body = |ui: &mut egui::Ui| {
                            a = ui.label("1").rect;
                            b = ui.label("2").rect;
                        };
                        if explicit {
                            ui.allocate_ui_with_layout(
                                size,
                                egui::Layout::top_down(egui::Align::Min),
                                body,
                            );
                        } else {
                            ui.allocate_ui(size, body);
                        }
                    });
                });
            });
            (a, b)
        }

        // 素の呼び方 — 2 つが**同じ行**に並ぶ (これが崩れの正体)。
        let (a, b) = two_labels(false);
        assert!(
            (a.top() - b.top()).abs() < 0.5 && b.left() > a.left(),
            "egui 0.29 の継承が変わった (崩れを再現できていない): a={a:?} b={b:?}"
        );
        // 明示すれば縦へ積む。
        let (a, b) = two_labels(true);
        assert!(
            b.top() >= a.bottom() - 0.5 && (a.left() - b.left()).abs() < 0.5,
            "縦積みになっていない: a={a:?} b={b:?}"
        );
    }

    /// **この画面では素の `allocate_ui` を使わない。**
    ///
    /// 縦積みは必ず `allocate_ui_with_layout` で明示する。判定に使う綴りは
    /// **実行時に組み立てる** — ソースへ書くと、この行自身を拾って
    /// 「わざと壊しても緑」の空回りする番人になる。テスト節は見ない
    /// (上の比較テストは、わざと素の呼び方をするため)。
    #[test]
    fn 縦積みは必ず明示する() {
        let src = include_str!("organization_board.rs").replace("\r\n", "\n");
        let prod = src.split("\n#[cfg(test)]\n").next().unwrap_or(src.as_str());
        let needle = format!("ui.{}(", "allocate_ui");
        let bad: Vec<usize> = prod
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim_start().starts_with("//"))
            .filter(|(_, l)| l.contains(&needle))
            .map(|(i, _)| i + 1)
            .collect();
        assert!(
            bad.is_empty(),
            "素の allocate_ui が残っている (行: {bad:?})。\
             親が横並びだと中身まで横に並ぶので allocate_ui_with_layout を使うこと"
        );
    }

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
