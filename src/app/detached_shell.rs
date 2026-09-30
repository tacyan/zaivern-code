use super::*;
use crate::keybinds;

impl ZaivernApp {
    pub(super) fn open_detached_shell(&mut self, sid: u64, ctx: &egui::Context) {
        if let Some(session) = self.agents.sessions.iter_mut().find(|s| s.id == sid) {
            let viewport = egui::ViewportId::from_hash_of(("session-shell", sid));
            session.shell_viewport = Some(viewport);
            session.preedit.clear();
            let focus_key = terminal::focused_terminal_key(ctx);
            ctx.data_mut(|data| {
                if data.get_temp::<u64>(focus_key) == Some(sid) {
                    data.remove::<u64>(focus_key);
                }
            });
            ctx.data_mut(|data| data.insert_temp(egui::Id::new(("shell-focus", sid)), true));
            ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Focus);
        }
    }

    pub(super) fn detached_shells_ui(&mut self, ctx: &egui::Context) {
        if !self
            .agents
            .sessions
            .iter()
            .any(|s| s.shell_viewport.is_some())
        {
            return;
        }
        let theme = self.theme.clone();
        let font = self.scaled_terminal_font();
        let find = self.keys.binding(BindAction::Find);
        let previous = self.keys.binding(BindAction::TermPrevPrompt);
        let next = self.keys.binding(BindAction::TermNextPrompt);
        for session in &mut self.agents.sessions {
            let Some(viewport) = session.shell_viewport else {
                continue;
            };
            let title = format!("Shell — {}", session.title);
            let mut close = false;
            ctx.show_viewport_immediate(
                viewport,
                egui::ViewportBuilder::default()
                    .with_title(&title)
                    .with_inner_size([960.0, 640.0]),
                |ctx, class| {
                    close |= ctx.input(|i| i.viewport().close_requested());
                    if close {
                        return;
                    }
                    let embedded = class == egui::ViewportClass::Embedded;
                    if embedded {
                        session.shell_viewport = Some(ctx.viewport_id());
                    }
                    let sid = session.id;
                    if !embedded {
                        keybinds::drop_duplicate_ime_text(ctx);
                        let chord_id = egui::Id::new(("shell-chord", sid));
                        let mut chord = ctx.data_mut(|data| {
                            data.remove_temp::<keybinds::ChordState>(chord_id)
                                .unwrap_or_default()
                        });
                        let ime = keybinds::ime_blocks_shortcuts_now(ctx);
                        chord.note_ime(ime, !ime);
                        ctx.input_mut(|i| chord.begin_frame(i));
                        let mut consume = |binding| {
                            !ime && ctx
                                .input_mut(|i| keybinds::consume_binding(i, binding, &mut chord))
                        };
                        if consume(find) {
                            session.search.open = true;
                            session.search.focus_pending = true;
                        }
                        let focus_key = terminal::focused_terminal_key(ctx);
                        if ctx.data(|d| d.get_temp::<u64>(focus_key)) == Some(sid) {
                            if consume(previous) {
                                session.shell_jump_prompt(false);
                            }
                            if consume(next) {
                                session.shell_jump_prompt(true);
                            }
                        }
                        if chord.is_waiting() {
                            crate::perf::repaint_after(
                                ctx,
                                std::time::Duration::from_secs_f64(
                                    chord.remaining(ctx.input(|i| i.time)).min(0.1),
                                ),
                                "detached_shell_chord",
                            );
                        }
                        ctx.data_mut(|data| data.insert_temp(chord_id, chord));
                    }
                    let mut content = |ui: &mut egui::Ui| {
                        if ui.button(tr("shell.return")).clicked() {
                            close = true;
                            return;
                        }
                        ui.separator();
                        let response = terminal::draw(ui, session, &theme, font, true, true, true);
                        if ctx.data_mut(|data| {
                            data.remove_temp::<bool>(egui::Id::new(("shell-focus", sid)))
                                .unwrap_or(false)
                        }) && !session.search.open
                        {
                            response.request_focus();
                        }
                    };
                    if embedded {
                        // 単一 viewport のバックエンドでも同じ PTY を表示する。
                        let mut open = true;
                        egui::Window::new(&title)
                            .id(egui::Id::new(("session-shell", sid)))
                            .open(&mut open)
                            .default_size([960.0, 640.0])
                            .show(ctx, &mut content);
                        close |= !open;
                    } else {
                        egui::CentralPanel::default().show(ctx, &mut content);
                    }
                },
            );
            if close {
                session.shell_viewport = None;
                session.preedit.clear();
                crate::perf::repaint(ctx, "detached_shell_close");
            } else {
                session.shell_viewport = Some(viewport);
            }
        }
    }
}
