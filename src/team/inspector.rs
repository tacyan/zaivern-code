//! Inspector — エージェント / タスクの詳細を右側に開く。
//!
//! **`ReportedSubAgent` の「端末を開く」は無効にして理由を出す。**
//! 押せるのに何も起きないボタンは、画面が嘘をついているのと同じ。

use eframe::egui;
use egui::RichText;

use crate::i18n::{tr, trf};
use crate::theme::Theme;

use super::model::*;
use super::panel::BoardAction;
use super::view_model::{self, TeamAgentView, TaskView, TeamSnapshot};

/// 1 つの一覧に出す行数の上限 (長文は Inspector でも切って hover で出す)。
pub const LIST_ROWS_MAX: usize = 12;

fn plain(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn section(ui: &mut egui::Ui, theme: &Theme, title_id: &str, items: &[String]) {
    // **空のセクションは高さを 1px も取らない。**
    if items.is_empty() {
        return;
    }
    ui.label(RichText::new(tr(title_id)).color(theme.text).strong());
    for it in items.iter().take(LIST_ROWS_MAX) {
        let t = plain(it);
        ui.label(RichText::new(format!("• {t}")).color(theme.text_dim))
            .on_hover_text(t);
    }
    let more = items.len().saturating_sub(LIST_ROWS_MAX);
    if more > 0 {
        ui.label(
            RichText::new(trf("team.inspector.more", &[("n", more.to_string())]))
                .color(theme.text_dim),
        );
    }
    ui.add_space(4.0);
}

/// Inspector を描く。開いていなければ 1 命令も走らない。
pub fn inspector_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    snap: &TeamSnapshot,
    agent: Option<&AgentId>,
    task: Option<TaskId>,
    note: &mut String,
    acts: &mut Vec<BoardAction>,
) {
    let a = agent.and_then(|id| snap.agents.iter().find(|x| x.id == *id));
    let t = task
        .or_else(|| a.and_then(|x| x.current_task))
        .and_then(|id| snap.tasks.iter().find(|x| x.id == id));
    if a.is_none() && t.is_none() {
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("team-inspector")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if let Some(a) = a {
                agent_section(ui, theme, snap, a, acts);
            }
            if let Some(t) = t {
                ui.separator();
                task_section(ui, theme, t, acts);
                ui.separator();
                edit_instruction(ui, theme, t.id, note, acts);
            }
        });
}

fn agent_section(
    ui: &mut egui::Ui,
    theme: &Theme,
    snap: &TeamSnapshot,
    a: &TeamAgentView,
    acts: &mut Vec<BoardAction>,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(a.state.glyph()).color(theme.accent));
        ui.label(RichText::new(&a.name).color(theme.text).strong());
    });
    egui::Grid::new("team-inspector-agent")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.label(tr("team.inspector.role"));
            ui.label(a.role.key());
            ui.end_row();
            ui.label(tr("team.inspector.kind"));
            ui.label(match a.kind {
                AgentKind::ManagedSession => tr("team.inspector.managed"),
                AgentKind::ReportedSubAgent => tr("team.inspector.reported"),
            });
            ui.end_row();
            if !a.provider.is_empty() {
                ui.label(tr("team.inspector.provider"));
                ui.label(&a.provider);
                ui.end_row();
            }
            if let Some(p) = &a.parent_id {
                ui.label(tr("team.inspector.parent"));
                ui.label(&p.0);
                ui.end_row();
            }
            ui.label(tr("team.inspector.state"));
            ui.label(a.state.key());
            ui.end_row();
            ui.label(tr("team.inspector.last_activity"));
            ui.label(view_model::elapsed_label(a.idle_secs));
            ui.end_row();
        });

    if !a.current_action.is_empty() {
        let act = plain(&a.current_action);
        ui.label(RichText::new(&act).color(theme.text_dim))
            .on_hover_text(act);
    }

    // 子エージェント
    let children: Vec<String> = snap
        .agents
        .iter()
        .filter(|c| c.parent_id.as_ref() == Some(&a.id))
        .map(|c| format!("{} {} — {}", c.state.glyph(), c.name, c.state.key()))
        .collect();
    section(ui, theme, "team.inspector.children", &children);

    section(ui, theme, "team.inspector.blockers", &a.blockers);

    ui.horizontal_wrapped(|ui| {
        match a.session_id {
            Some(sid) if a.can_open_terminal => {
                if ui.button(tr("team.btn.open_terminal")).clicked() {
                    acts.push(BoardAction::OpenTerminal(sid));
                }
            }
            _ => {
                // **押せるのに何も起きないボタンを作らない。**
                let why = if a.kind == AgentKind::ReportedSubAgent {
                    tr("team.terminal.reported_no_terminal")
                } else {
                    tr("team.terminal.not_started")
                };
                ui.add_enabled(false, egui::Button::new(tr("team.btn.open_terminal")))
                    .on_disabled_hover_text(why);
            }
        }
        if let Some(tid) = a.current_task {
            if ui.button(tr("team.btn.retry")).clicked() {
                acts.push(BoardAction::Retry(tid));
            }
            if ui.button(tr("team.btn.reassign")).clicked() {
                acts.push(BoardAction::Reassign(tid));
            }
        }
    });
}

fn task_section(ui: &mut egui::Ui, theme: &Theme, t: &TaskView, acts: &mut Vec<BoardAction>) {
    ui.label(
        RichText::new(format!("#{} {}", t.id, t.title))
            .color(theme.text)
            .strong(),
    );
    egui::Grid::new("team-inspector-task")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.label(tr("team.inspector.state"));
            ui.label(t.state.key());
            ui.end_row();
            ui.label(tr("team.inspector.role"));
            ui.label(t.role.key());
            ui.end_row();
            if let Some(a) = &t.assigned_agent {
                ui.label(tr("team.inspector.assignee"));
                ui.label(&a.0);
                ui.end_row();
            }
            ui.label(tr("team.inspector.attempts"));
            ui.label(t.attempts.to_string());
            ui.end_row();
            ui.label(tr("team.inspector.validation"));
            ui.label(if !t.validation_ran {
                tr("team.inspector.not_run")
            } else if t.validation_ok {
                tr("team.inspector.passed")
            } else {
                // **なぜ通らなかったのかまで出す。** 時間切れと実装の失敗を
                // 同じ「失敗」で塗ると、直しようが無い。
                match t.validation_result {
                    Some(ValidationOutcome::TimedOut) => tr("team.validation.timed_out"),
                    Some(ValidationOutcome::Cancelled) => tr("team.validation.cancelled"),
                    Some(ValidationOutcome::SpawnFailed) => tr("team.validation.spawn_failed"),
                    Some(ValidationOutcome::RunnerDisconnected) => {
                        tr("team.validation.runner_disconnected")
                    }
                    _ => tr("team.inspector.failed"),
                }
            });
            ui.end_row();
            ui.label(tr("team.inspector.review"));
            ui.label(match t.review_verdict {
                Some(ReviewVerdict::Approve) => tr("team.review.approved"),
                Some(ReviewVerdict::RequestChanges) => tr("team.review.changes"),
                None => tr("team.inspector.not_run"),
            });
            ui.end_row();
        });

    section(ui, theme, "team.inspector.files", &t.files);
    section(
        ui,
        theme,
        "team.inspector.acceptance",
        &t.acceptance_criteria,
    );
    section(
        ui,
        theme,
        "team.inspector.commands",
        &t.validation_commands,
    );
    section(ui, theme, "team.inspector.findings", &t.review_findings);
    section(ui, theme, "team.inspector.context", &t.context);
    section(ui, theme, "team.inspector.blockers", &t.blockers);
    if !t.last_summary.is_empty() {
        ui.label(RichText::new(tr("team.inspector.last_report")).color(theme.text).strong());
        let sum = plain(&t.last_summary);
        ui.label(RichText::new(&sum).color(theme.text_dim))
            .on_hover_text(sum);
    }
    ui.horizontal_wrapped(|ui| {
        if ui.button(tr("team.btn.retry")).clicked() {
            acts.push(BoardAction::Retry(t.id));
        }
        if ui.button(tr("team.btn.reassign")).clicked() {
            acts.push(BoardAction::Reassign(t.id));
        }
    });
}

/// **Edit Instruction** — 次の担当へ渡す追加の指示を足す。
///
/// エージェントへ直接打つのではなく**タスクの文脈へ足す**のが要で、
/// こうしておくと担当が入れ替わっても指示が引き継がれる (直接打つと、
/// そのセッションが死んだ瞬間に消える)。
fn edit_instruction(
    ui: &mut egui::Ui,
    theme: &Theme,
    task: TaskId,
    note: &mut String,
    acts: &mut Vec<BoardAction>,
) {
    ui.label(
        RichText::new(tr("team.inspector.edit_instruction"))
            .color(theme.text)
            .strong(),
    );
    ui.add(
        egui::TextEdit::multiline(note)
            .desired_rows(3)
            .desired_width(ui.available_width())
            .hint_text(tr("team.inspector.edit_hint")),
    );
    let empty = note.trim().is_empty();
    // **空の指示を送れるボタンを出さない** (押せるのに何も起きない)。
    if ui
        .add_enabled(!empty, egui::Button::new(tr("team.inspector.add_context")))
        .clicked()
    {
        acts.push(BoardAction::AddContext {
            task,
            text: note.trim().to_string(),
        });
        note.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 制御文字を落とす() {
        assert_eq!(plain("a\r\nb"), "a b");
    }

    #[test]
    fn 一覧の上限がある() {
        assert!(LIST_ROWS_MAX <= 20);
    }
}
