//! 🏛 Team 制御面と既存 app の**橋**。
//!
//! ## なぜ薄いのか
//!
//! Team の判断は全部 `src/team/` にあり、ここは既存の仕組みへ繋ぐだけ:
//!
//! * セッションの観測 → [`coordinator_state`](super::coordinator_state) の結果を渡す
//! * 起動 → 既存の [`launch_preset_with`](ZaivernApp::launch_preset_with)
//! * 指示 → 既存の送信経路 1 本 ([`queue_submit`](ZaivernApp::queue_submit))
//! * 停止 → 既存の承認ゲート越しの停止提案
//! * 端末を開く → 既存の [`focus_agent_in_place`](ZaivernApp::focus_agent_in_place)
//!
//! **並行実装を作らない**のがこのファイルの存在理由で、ここに判断を書き足し
//! たくなったら `src/team/` 側に置くべき合図。
//!
//! ## 状態の置き場
//!
//! Team の状態は `ZaivernApp` のフィールドではなく
//! [`crate::features::team::imp::panel`] の `thread_local!` に置いてある。
//! `ZaivernApp` の構造体と初期化は全ブランチが取り合う共有面なので、欄を
//! 増やさずに済むほうを採った (UI スレッドからしか触らないので安全)。

use std::time::Instant;

use eframe::egui;

use crate::features::team::imp::model::{SessionId, TaskId, ValidationRun};
use crate::features::team::imp::panel::{
    self, BoardAction, BoardTab, RestorePrompt, SessionInput, SCAN_COLS, SCAN_ROWS,
};
use crate::features::team::imp::runtime::{RunOptions, TeamAction};
use crate::features::team::imp::{inspector, launch, organization_board};
use crate::i18n::{tr, trf};

use super::ZaivernApp;

impl ZaivernApp {
    /// 🏛 Team 画面を開く / 閉じる (コマンドパレットから)。
    pub(crate) fn toggle_team_board(&mut self) {
        let ws = self.agent_cwd();
        panel::with_panel(|p| {
            p.open = !p.open;
            if p.open {
                p.attach_workspace(&ws);
                p.mark_dirty();
            }
        });
    }

    /// 🏛 New Team Run のフォームを開く。
    pub(crate) fn open_team_new_run(&mut self) {
        let ws = self.agent_cwd();
        panel::with_panel(|p| {
            p.open = true;
            p.attach_workspace(&ws);
            p.form.open = true;
            p.form.error.clear();
            p.mark_dirty();
        });
    }

    /// 毎フレーム呼ばれる Team の駆動。**閉じていても Run が動いていれば進める**
    /// (画面を閉じただけで開発が止まったら困る)。ただし
    /// **Run が無いときは 1 命令も走らない**ので、アイドルのコストはゼロ。
    pub(crate) fn team_tick(&mut self, ctx: &egui::Context) {
        // 1) `zai team run` からの起動要求を拾う (**1 回だけ**)。
        self.team_take_launch_request();

        let has_run = panel::with_panel(|p| p.has_run());
        if !has_run {
            self.team_board_ui(ctx);
            return;
        }

        // 2) 走査は間隔を空ける (毎フレーム 64 体の画面を舐めない)。
        let due = panel::with_panel(|p| p.scan_due(Instant::now()));
        if due {
            let rows = self.team_session_inputs();
            let now = crate::features::team::imp::model::now_secs();
            panel::with_panel(|p| p.pump_sessions(rows, now));
        }

        // 3) Runtime が求めた副作用を実行する (**描画の外**)。
        self.team_run_effects(ctx);

        // 4) 保存が要るなら保存する。
        panel::with_panel(|p| p.save_if_needed());

        // 5) 画面。
        self.team_board_ui(ctx);

        // 6) **走っている間だけ**自分で次のフレームを頼む。
        //
        //    調停ループは入力が無いと進まないので、これが無いと「マウスを
        //    動かしたときだけ開発が進む」になる。逆に Run が無いときは
        //    1 回も頼まない (設計原則 3: アイドルのコストはゼロ)。
        if self.team_is_active() {
            // **出所を記録して頼む。** `ZAIVERN_PERF=1` の集計で
            // 「誰が再描画を要求したか」が見えないと、アイドルの費用を
            // 数字で追えない (設計原則 3 はここで守る)。
            crate::perf::repaint_after(ctx, panel::SCAN_INTERVAL, "team");
        }
    }

    /// `zai team run` が投函した起動要求を拾う。
    ///
    /// **一度しか処理しない** ([`launch::take`] が拾うと同時に消す)。
    fn team_take_launch_request(&mut self) {
        let ws = self.agent_cwd();
        let now = crate::features::team::imp::model::now_secs();
        let Some(req) = launch::take(&ws, now) else {
            return;
        };
        let opts = RunOptions {
            run_id: format!("run-{}", req.requested_at),
            spec_source: req.spec_path.display().to_string(),
            agent_count: req.agent_count,
            ..RunOptions::default()
        };
        let auto = req.auto_start;
        let result = panel::with_panel(|p| {
            p.open = true;
            p.attach_workspace(&req.workspace_root);
            p.form.open = false;
            p.tab = BoardTab::Organization;
            let r = p.plan(&req.spec_text, &opts.spec_source.clone(), opts);
            if r.is_ok() && auto {
                // `--yes` は **Start Team の確認だけ**を省く。
                p.act(TeamAction::Start);
            }
            r
        });
        match result {
            Ok(()) => self.toast(
                trf(
                    "team.toast.plan_ready",
                    &[("spec", req.spec_path.display().to_string())],
                ),
                true,
            ),
            Err(e) => {
                panel::with_panel(|p| p.notice = e.clone());
                self.toast(e, false);
            }
        }
    }

    /// いま生きているセッションを Team へ渡す形にする。
    fn team_session_inputs(&self) -> Vec<SessionInput> {
        self.agents
            .sessions
            .iter()
            .map(|s| SessionInput {
                id: s.id,
                title: s.title.clone(),
                provider: crate::agents::spec_for_command(&s.command)
                    .map(|x| x.bin.to_string())
                    .unwrap_or_else(|| s.preset_name.clone()),
                // **状態の真実は既存側。** ここで別の判定を作らない。
                state: super::coordinator_state(
                    s.running(),
                    s.attention,
                    s.rate_limited.is_some(),
                    self.supervisor.state_of(s.id),
                ),
                tail: s.screen_tail_lines(SCAN_ROWS, SCAN_COLS),
            })
            .collect()
    }

    /// Runtime が求めた副作用を実行する。
    fn team_run_effects(&mut self, ctx: &egui::Context) {
        // ── エージェントの起動 ──
        let launches = panel::with_panel(|p| p.take_launches());
        for spec in launches {
            match self.team_launch_agent(ctx) {
                Some(session) => {
                    panel::with_panel(|p| p.bind_session(&spec.agent_id, session));
                    self.toast(
                        trf("team.toast.agent_started", &[("name", spec.name.clone())]),
                        true,
                    );
                }
                None => {
                    let why = tr("team.err.no_agent_preset");
                    panel::with_panel(|p| p.note_launch_failed(&spec.agent_id, &why));
                    self.toast(why, false);
                }
            }
        }

        // ── 指示の送信 (**既存の送信経路 1 本**を通す) ──
        let instructions = panel::with_panel(|p| p.take_instructions());
        for (session, text) in instructions {
            // 起動直後は Idle を待ってから送る (Ink 系 TUI の取りこぼし対策は
            // `submit.rs` が持っている)。
            self.queue_submit(crate::submit::Job::deferred(session, text, true));
        }

        // ── 停止 (承認済みのものだけがここへ来る) ──
        let stops = panel::with_panel(|p| p.take_stops());
        for session in stops {
            if let Some(i) = self.agents.sessions.iter().position(|s| s.id == session) {
                self.close_agent(i);
            }
        }

        // ── 検証コマンドの実行 ──
        //
        // **UI スレッドでブロッキングしない。** 裏のスレッドで走らせて、
        // 終わったら結果だけを戻す。
        let validations = panel::with_panel(|p| p.take_validations());
        for v in validations {
            self.team_spawn_validation(v);
        }
        panel::with_panel(|p| p.collect_validations());
    }

    /// エージェントを 1 体起こす。戻りは新しいセッション ID。
    fn team_launch_agent(&mut self, ctx: &egui::Context) -> Option<SessionId> {
        // **AI CLI のプリセットを選ぶ。** 素のシェルではチームにならない。
        let idx = self
            .cfg
            .agents
            .iter()
            .position(|p| crate::agents::spec_for_command(&p.command).is_some())?;
        let before: std::collections::HashSet<SessionId> =
            self.agents.sessions.iter().map(|s| s.id).collect();
        self.launch_preset(idx, ctx);
        self.agents
            .sessions
            .iter()
            .map(|s| s.id)
            .find(|id| !before.contains(id))
    }

    /// 検証を裏で走らせる。
    fn team_spawn_validation(&mut self, v: crate::features::team::imp::runtime::ValidationSpec) {
        let (tx, rx) = std::sync::mpsc::channel::<(TaskId, Vec<ValidationRun>)>();
        let task = v.task;
        let cwd = v.cwd.clone();
        let cmds = v.commands.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("zai-team-validate-{task}"))
            .spawn(move || {
                let runs: Vec<ValidationRun> = cmds
                    .iter()
                    .map(|c| launch::run_validation_command(c, &cwd))
                    .collect();
                let _ = tx.send((task, runs));
            });
        match spawned {
            Ok(_) => panel::with_panel(|p| p.watch_validation(rx)),
            Err(e) => {
                // スレッドを作れないなら「実行できなかった」として戻す
                // (黙って未実行のままにすると永久に待つ)。
                let runs = v
                    .commands
                    .iter()
                    .map(|c| ValidationRun {
                        command: c.clone(),
                        exit_code: 126,
                    })
                    .collect();
                panel::with_panel(|p| p.note_validation(task, runs));
                self.toast(
                    trf("team.err.validation_spawn", &[("e", e.to_string())]),
                    false,
                );
            }
        }
    }

    /// 🏛 Team 画面。**閉じているときは 1 命令も走らない。**
    pub(crate) fn team_board_ui(&mut self, ctx: &egui::Context) {
        let open = panel::with_panel(|p| p.open);
        if !open {
            return;
        }
        let now = crate::features::team::imp::model::now_secs();
        let theme = self.theme.clone();
        let acts = panel::with_panel(|p| {
            p.refresh_snapshot(now);
            let mut form = p.form.clone();
            let mut acts = organization_board::board_window(
                ctx,
                &theme,
                p.open,
                p.snapshot(),
                p.tab,
                &mut form,
                p.restore,
                p.selected_agent.as_ref(),
                &p.notice,
            );
            p.form = form;
            // Inspector は別窓 (盤面を狭めない)。
            if p.inspector_open {
                // 入力欄は借用を切ってから渡す (スナップショットは不変借用、
                // 入力欄は可変借用なので、同時には持てない)。
                let mut note = std::mem::take(&mut p.inspector_note);
                if let Some(snap) = p.snapshot() {
                    let mut win_open = true;
                    let sel = p.selected_agent.clone();
                    let task = p.selected_task;
                    egui::Window::new(tr("team.inspector.title"))
                        .open(&mut win_open)
                        .default_width(360.0)
                        .default_height(520.0)
                        .anchor(egui::Align2::RIGHT_CENTER, [-12.0, 0.0])
                        .show(ctx, |ui| {
                            inspector::inspector_ui(
                                ui,
                                &theme,
                                snap,
                                sel.as_ref(),
                                task,
                                &mut note,
                                &mut acts,
                            );
                        });
                    if !win_open {
                        p.inspector_open = false;
                    }
                }
                p.inspector_note = note;
            }
            acts
        });
        for a in acts {
            self.team_apply_board_action(a);
        }
    }

    /// 画面からの要求を実行する。
    fn team_apply_board_action(&mut self, a: BoardAction) {
        match a {
            BoardAction::Close => panel::with_panel(|p| p.open = false),
            BoardAction::SwitchTab(t) => panel::with_panel(|p| p.tab = t),
            BoardAction::Start => panel::with_panel(|p| p.act(TeamAction::Start)),
            BoardAction::Pause => panel::with_panel(|p| p.act(TeamAction::Pause)),
            BoardAction::Resume => panel::with_panel(|p| p.act(TeamAction::Resume)),
            // **Stop は承認ゲートを通る。** Runtime が Decision を立てるので、
            // ここでは kill しない。
            BoardAction::Stop => panel::with_panel(|p| p.act(TeamAction::Stop)),
            BoardAction::Approve(id) => {
                panel::with_panel(|p| p.act(TeamAction::ApproveDecision(id)))
            }
            BoardAction::Reject(id) => panel::with_panel(|p| p.act(TeamAction::RejectDecision(id))),
            BoardAction::Retry(t) => panel::with_panel(|p| p.act(TeamAction::RetryTask(t))),
            BoardAction::Reassign(t) => panel::with_panel(|p| p.act(TeamAction::ReassignTask(t))),
            BoardAction::Select(id) => panel::with_panel(|p| {
                p.selected_agent = Some(id);
                p.selected_task = None;
                p.inspector_open = true;
            }),
            BoardAction::AddContext { task, text } => {
                panel::with_panel(|p| p.act(TeamAction::AddContext { task, text }));
            }
            BoardAction::SelectTask(t) => panel::with_panel(|p| {
                p.selected_task = Some(t);
                p.inspector_open = true;
            }),
            BoardAction::OpenTerminal(sid) => self.team_open_terminal(sid),
            BoardAction::OpenNewRun => panel::with_panel(|p| {
                p.form.open = true;
                p.form.error.clear();
            }),
            BoardAction::PlanFromForm => self.team_plan_from_form(),
            BoardAction::ResumeRun => {
                let r = panel::with_panel(|p| {
                    if p.restore == RestorePrompt::ConfirmDiscard {
                        p.restore = RestorePrompt::Found;
                        return Ok(());
                    }
                    p.restore_run(false)
                });
                if let Err(e) = r {
                    self.toast(e, false);
                }
            }
            BoardAction::OpenReadOnly => {
                if let Err(e) = panel::with_panel(|p| p.restore_run(true)) {
                    self.toast(e, false);
                }
            }
            BoardAction::DiscardRun => {
                // **確認を挟む。** 消したら戻せない。
                let confirmed = panel::with_panel(|p| p.restore == RestorePrompt::ConfirmDiscard);
                if !confirmed {
                    panel::with_panel(|p| p.restore = RestorePrompt::ConfirmDiscard);
                    return;
                }
                match panel::with_panel(|p| p.discard_run()) {
                    Ok(n) => self.toast(trf("team.toast.discarded", &[("n", n.to_string())]), true),
                    Err(e) => self.toast(e, false),
                }
            }
        }
    }

    /// フォームの内容で計画する。
    fn team_plan_from_form(&mut self) {
        let ws = self.agent_cwd();
        let form = panel::with_panel(|p| p.form.clone());
        let (spec_text, source) = if form.from_file {
            let path = if std::path::Path::new(&form.spec_path).is_absolute() {
                std::path::PathBuf::from(&form.spec_path)
            } else {
                ws.join(&form.spec_path)
            };
            // **ワークスペース境界と UTF-8 とサイズを必ず確かめる。**
            match launch::build(&ws, &path, form.agents, false) {
                Ok(req) => (req.spec_text, req.spec_path.display().to_string()),
                Err(e) => {
                    panel::with_panel(|p| p.form.error = e.detail());
                    return;
                }
            }
        } else {
            (form.spec_text.clone(), tr("team.form.direct_source"))
        };
        let opts = RunOptions {
            run_id: format!("run-{}", crate::features::team::imp::model::now_secs()),
            spec_source: source.clone(),
            agent_count: form.agents,
            max_attempts: form.max_attempts,
            review_required: form.review_required,
        };
        let r = panel::with_panel(|p| {
            let r = p.plan(&spec_text, &source, opts);
            if r.is_ok() {
                p.form.open = false;
                p.form.error.clear();
                p.tab = BoardTab::Organization;
            }
            r
        });
        match r {
            Ok(()) => self.toast(tr("team.toast.plan_preview"), true),
            Err(e) => panel::with_panel(|p| p.form.error = e),
        }
    }

    /// エージェントの端末を開く (既存の選択経路をそのまま使う)。
    fn team_open_terminal(&mut self, session: SessionId) {
        let Some(i) = self.agents.sessions.iter().position(|s| s.id == session) else {
            self.toast(tr("team.err.session_gone"), false);
            return;
        };
        self.focus_agent_in_place(i);
        self.agents.panel_open = true;
    }

    /// Team が動いているか (再描画の判断に使う)。
    ///
    /// **終わった Goal では 1 回も再描画を頼まない。** 完了した盤面を
    /// 開いたまま放置しても、アイドルのコストはゼロに戻る (設計原則 3)。
    pub(crate) fn team_is_active(&self) -> bool {
        panel::with_panel(|p| {
            p.has_run() && !p.is_read_only() && p.goal_status().is_some_and(|g| !g.is_terminal())
        })
    }
}
