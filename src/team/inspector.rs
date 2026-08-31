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
    // **相手が 1 体も居ないときだけ閉じる。** 誰も選んでいない状態でも
    // Inspector から選び直せるようにしておく (毎回盤面へ戻らせない)。
    if a.is_none() && t.is_none() && !snap.agents.iter().any(|x| x.can_open_terminal) {
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("team-inspector")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            agent_picker(ui, theme, snap, a, acts);
            if let Some(a) = a {
                ui.separator();
                agent_section(ui, theme, snap, a, acts);
            }
            if let Some(t) = t {
                ui.separator();
                task_section(ui, theme, t, acts);
            }
            // **指示欄は常に出す。** タスクが選ばれていなくても、動いている
            // 相手には口を出せる (Team Lead など、タスクを持たない担当が居る)。
            ui.separator();
            instruction_box(ui, theme, a, t.map(|t| t.id), note, acts);
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
    // **値の列は残り幅で切る。** 提供元や親の ID は任意長なので、
    // 切らないと細い Inspector で右へはみ出す (実測 18px)。
    let value = |ui: &mut egui::Ui, s: &str| {
        ui.add(egui::Label::new(s).truncate()).on_hover_text(s);
    };
    egui::Grid::new("team-inspector-agent")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .max_col_width((ui.available_width() * 0.55).max(80.0))
        .show(ui, |ui| {
            ui.label(tr("team.inspector.role"));
            value(ui, a.role.key());
            ui.end_row();
            ui.label(tr("team.inspector.kind"));
            ui.label(match a.kind {
                AgentKind::ManagedSession => tr("team.inspector.managed"),
                AgentKind::ReportedSubAgent => tr("team.inspector.reported"),
            });
            ui.end_row();
            if !a.provider.is_empty() {
                ui.label(tr("team.inspector.provider"));
                value(ui, &a.provider);
                ui.end_row();
            }
            if let Some(p) = &a.parent_id {
                ui.label(tr("team.inspector.parent"));
                value(ui, &p.0);
                ui.end_row();
            }
            ui.label(tr("team.inspector.state"));
            value(ui, a.state.key());
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
    // **落ちた検証の出力を人にも見せる。** ここが無いと、直せなかった
    // ときに人が同じコマンドを手で打ち直して確かめることになる。
    section(
        ui,
        theme,
        "team.inspector.diagnostics",
        &t.validation_diagnostics,
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
/// 宛先エージェントの切り替え。
///
/// **盤面まで戻らずに相手を変えられるようにする。** 途中で口を出すとき、
/// 「どの子に言うか」を選ぶのに毎回カードを探させない。
fn agent_picker(
    ui: &mut egui::Ui,
    theme: &Theme,
    snap: &TeamSnapshot,
    current: Option<&TeamAgentView>,
    acts: &mut Vec<BoardAction>,
) {
    // 端末を持っている相手だけを出す (押せるのに届かない選択肢を作らない)。
    let targets: Vec<&TeamAgentView> = snap.agents.iter().filter(|a| a.can_open_terminal).collect();
    if targets.is_empty() {
        return;
    }
    // **見出しと選択欄を 1 行に並べない。** 並べると Inspector を細くした
    // ときに選択欄が右へ 20〜30px はみ出す (`ComboBox::width` が縛るのは
    // 中身だけで、矢印と余白はその外側に足される)。
    // `inspectorはどの幅でも収まりエージェント選択を出す` が捕まえた。
    ui.label(RichText::new(tr("team.inspector.target")).color(theme.text_dim));
    let label = current
        .map(|a| format!("{} {}", a.state.glyph(), a.name))
        .unwrap_or_else(|| tr("team.inspector.pick_agent"));
    // 矢印と左右の余白のぶんを引く (下限は、名前が 1 文字も読めなくならない線)。
    let w = (ui.available_width() - 32.0).max(96.0);
    egui::ComboBox::from_id_salt("team-inspector-target")
        .width(w)
        .selected_text(label)
        .show_ui(ui, |ui| {
            for a in targets {
                let on = current.is_some_and(|c| c.id == a.id);
                if ui
                    .selectable_label(on, format!("{} {}", a.state.glyph(), a.name))
                    .clicked()
                {
                    acts.push(BoardAction::Select(a.id.clone()));
                }
            }
        });
}

/// 指示欄。**送り先が 2 通りあることを、ボタンの側で見せる。**
///
/// * 「いま送る」 — 動いている端末へ 1 回流す ([`BoardAction::InstructAgent`])
/// * 「次の配布に足す」 — タスクの文脈へ残す ([`BoardAction::AddContext`])
///
/// どちらも押せないときは**理由をホバーで出す** (グレーアウトだけだと、
/// なぜ押せないのかがどこにも出ない)。
fn instruction_box(
    ui: &mut egui::Ui,
    theme: &Theme,
    agent: Option<&TeamAgentView>,
    task: Option<TaskId>,
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
    let live = agent.filter(|a| a.can_open_terminal);
    ui.horizontal_wrapped(|ui| {
        let why_send = if empty {
            tr("team.inspector.why_empty")
        } else if live.is_none() {
            tr("team.inspector.why_no_terminal")
        } else {
            String::new()
        };
        let send = ui.add_enabled(
            !empty && live.is_some(),
            egui::Button::new(tr("team.inspector.send_now")),
        );
        let send = if why_send.is_empty() {
            send.on_hover_text(tr("team.inspector.send_now_hint"))
        } else {
            send.on_disabled_hover_text(why_send)
        };
        if send.clicked() {
            if let Some(a) = live {
                acts.push(BoardAction::InstructAgent {
                    agent: a.id.clone(),
                    text: note.trim().to_string(),
                });
                note.clear();
            }
        }

        let why_ctx = if empty {
            tr("team.inspector.why_empty")
        } else if task.is_none() {
            tr("team.inspector.why_no_task")
        } else {
            String::new()
        };
        let add = ui.add_enabled(
            !empty && task.is_some(),
            egui::Button::new(tr("team.inspector.add_context")),
        );
        let add = if why_ctx.is_empty() {
            add.on_hover_text(tr("team.inspector.add_context_hint"))
        } else {
            add.on_disabled_hover_text(why_ctx)
        };
        if add.clicked() {
            if let Some(t) = task {
                acts.push(BoardAction::AddContext {
                    task: t,
                    text: note.trim().to_string(),
                });
                note.clear();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Inspector は、相手を選ぶ口と指示の口を必ず出す。**
    ///
    /// 実 egui で描いて (1) 落ちないこと (2) 渡した幅に収まること を見る。
    /// 「送る先が 2 通りある」ことは押せる・押せないで見せているので、
    /// 中身が幅からあふれると理由のホバーごと見切れる。
    #[test]
    fn inspectorはどの幅でも収まりエージェント選択を出す() {
        // **端末を結び付けてから描く。** 端末を持たない相手しか居ないと
        // 宛先の一覧そのものが出ないので、収まりを確かめたことにならない。
        let mut rt = super::super::runtime_tests::started(4);
        let ids: Vec<AgentId> = rt.agents().iter().map(|a| a.id.clone()).collect();
        for (i, id) in ids.iter().enumerate() {
            rt.bind_session(id, 900 + i as SessionId, None);
        }
        let snap = view_model::snapshot(&rt, 100);
        let theme = crate::theme::all().remove(0);
        let first = snap.agents[0].id.clone();
        // 相手なし / 相手あり の両方。
        for agent in [None, Some(&first)] {
            for width in [280.0_f32, 360.0, 520.0] {
                let ctx = egui::Context::default();
                let mut note = String::new();
                let mut acts = Vec::new();
                let mut inner = 0.0_f32;
                // **画面の大きさを渡す。** 既定の `RawInput` は画面を
                // 10000×10000 と見なすので、`auto_shrink` を切った
                // ScrollArea がそこまで広がり、幅の検査が意味を失う。
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::pos2(0.0, 0.0),
                        egui::vec2(width, 900.0),
                    )),
                    ..Default::default()
                };
                let _ = ctx.run(input, |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.set_max_width(width);
                        inspector_ui(ui, &theme, &snap, agent, None, &mut note, &mut acts);
                        inner = ui.min_rect().width();
                    });
                });
                assert!(
                    inner <= width + 1.0,
                    "幅 {width} に対し Inspector が {inner} まではみ出した (agent={})",
                    agent.is_some()
                );
            }
        }
    }

    /// **指示欄は、相手が選ばれていなくても出す。**
    ///
    /// タスクを持たない担当 (Team Lead など) にも口を出せる必要がある。
    /// 相手が 1 体も居ないときだけ、Inspector ごと閉じる。
    #[test]
    fn 相手が居なければinspectorは何も描かない() {
        let theme = crate::theme::all().remove(0);
        // 相手が 1 体も居ない盤面 (実物から作って空にする — 第 2 の作り方を作らない)。
        let rt = super::super::runtime_tests::started(4);
        let mut empty = view_model::snapshot(&rt, 100);
        empty.agents.clear();
        empty.tasks.clear();
        let ctx = egui::Context::default();
        let mut note = String::new();
        let mut acts = Vec::new();
        let mut drew = true;
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let before = ui.min_rect();
                inspector_ui(ui, &theme, &empty, None, None, &mut note, &mut acts);
                drew = ui.min_rect() != before;
            });
        });
        assert!(!drew, "相手が 1 体も居ないのに何か描いた");
        assert!(acts.is_empty(), "何も選べないのに操作を返した");
    }

    #[test]
    fn 制御文字を落とす() {
        assert_eq!(plain("a\r\nb"), "a b");
    }

    #[test]
    fn 一覧の上限がある() {
        assert!(LIST_ROWS_MAX <= 20);
    }
}
