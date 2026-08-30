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
        // **断られたら黙らない。** 実行中の Run があるときは workspace を
        // 切り替えないので、画面には前の Run が出たままになる。理由を出す。
        let refused = panel::with_panel(|p| {
            p.open = !p.open;
            if !p.open {
                return None;
            }
            let r = p.attach_workspace(&ws).err();
            p.mark_dirty();
            r
        });
        if let Some(why) = refused {
            self.toast(why, false);
        }
    }

    /// 🏛 New Team Run のフォームを開く。
    pub(crate) fn open_team_new_run(&mut self) {
        let ws = self.agent_cwd();
        let refused = panel::with_panel(|p| {
            p.open = true;
            let r = p.attach_workspace(&ws).err();
            p.form.open = true;
            p.form.error = r.clone().unwrap_or_default();
            p.mark_dirty();
            r
        });
        if let Some(why) = refused {
            self.toast(why, false);
        }
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
        // **毎フレーム `stat` を撃たない。** 画面が動いている間はここが
        // 60fps で呼ばれる (設計原則 3: アイドル時のコストはゼロ)。
        if !panel::with_panel(|p| p.launch_poll_due(Instant::now())) {
            return;
        }
        let ws = self.agent_cwd();
        // **実行中の Run を別のフォルダの要求で潰さない。** 投函は拾うと
        // 消えるので、拾ってから断ると要求ごと失われる (利用者は
        // `zai team run` をもう一度打つしかない)。TTL の間は置いておく。
        // **同じフォルダでも拾わない。** 拾ったあと `plan` が断るので、
        // 要求だけが消えて何も起きない (利用者はもう一度打つしかない)。
        if panel::with_panel(|p| p.live_work().is_busy()) {
            return;
        }
        let now = crate::features::team::imp::model::now_secs();
        // 根は明示して渡す (既定の決め所は persistence::default_home の 1 か所)。
        let root = crate::features::team::imp::persistence::default_home();
        let Some(req) = launch::take_in(&root, &ws, now) else {
            return;
        };
        let opts = RunOptions {
            spec_source: req.spec_path.display().to_string(),
            agent_count: req.agent_count,
            ..RunOptions::default()
        };
        let auto = req.auto_start;
        let result = panel::with_panel(|p| {
            p.open = true;
            // **要求の中の workspace を attach しない。** 未信頼データに
            // 置き場と cwd を決めさせると、投函箱を書き換えるだけで
            // 「別のフォルダを Team Run にする」ができてしまう。
            // 権限を持つのは**いま開いている workspace** だけ
            // (`take_in` も同じ値で境界を確かめている)。
            // **実行中の Run があるなら乗っ取らない。** `zai team run` を
            // 二重に叩いても、動いているチームを別の計画で潰さない。
            p.attach_workspace(&ws)?;
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
        //
        // **成功したときだけ ACK を返す。** 返さない限り Runtime は「済んだ」
        // と見なさないので、ここで落ちても次の起動で撃ち直される。
        let launches = panel::with_panel(|p| p.take_launches());
        for (key, spec) in launches {
            // **起こす前に、もう居ないかを見る。** 起動に成功してから
            // 結び付けが保存されるまでの間に落ちると、記録には残らないのに
            // セッションだけが残る (Zaivern は自分のセッションを復元するので、
            // 次の起動でも生きている)。そこへ素直に起こし直すと、同じ
            // logical agent が 2 体になる。
            if let Some((session, identity)) = self.team_adopt_session(&spec) {
                panel::with_panel(|p| {
                    p.bind_session(&spec.agent_id, session, Some(identity));
                    p.ack_done(&key);
                });
                self.toast(
                    trf("team.toast.agent_adopted", &[("name", spec.name.clone())]),
                    true,
                );
                continue;
            }
            match self.team_launch_agent(&spec, ctx) {
                Some(session) => {
                    // **目印を一緒に覚える。** 生ログのパスは復元しても
                    // 同じものを使う (`session::AgentSessionRec::log_file`)
                    // ので、次の起動でも同じセッションだと分かる。
                    let identity = self.team_session_identity(session);
                    panel::with_panel(|p| {
                        p.bind_session(&spec.agent_id, session, identity);
                        p.ack_done(&key);
                    });
                    self.toast(
                        trf("team.toast.agent_started", &[("name", spec.name.clone())]),
                        true,
                    );
                }
                None => {
                    let why = tr("team.err.no_agent_preset");
                    panel::with_panel(|p| {
                        p.note_launch_failed(&spec.agent_id, &why);
                        p.ack_failed(&key);
                    });
                    self.toast(why, false);
                }
            }
        }

        // ── 指示の送信 (**既存の送信経路 1 本**を通す) ──
        let instructions = panel::with_panel(|p| p.take_instructions());
        for (key, task, session, text) in instructions {
            // 起動直後は Idle を待ってから送る (Ink 系 TUI の取りこぼし対策は
            // `submit.rs` が持っている)。積めなければ失敗として返し、
            // 次の tick でもう一度出させる。
            // **止められた理由が「方針」なら撃ち直さない。** コスト上限は
            // 次の tick でも同じ理由で止まるので、送り直すぶんだけ同じ
            // トーストが出続けて前に進まない。人が手当てできる形へ上げる
            // (既存のコスト上限判定をそのまま使う — 第 2 の判定を作らない)。
            let blocked = self.cost_block_reason();
            let queued = if blocked.is_some() {
                false
            } else {
                // **配達の結末を受け取れるように目印を付ける。**
                // 冪等キーに Run を添えるだけ (第 2 の ID 体系を作らない)。
                // Run を添えないと、Run を作り直したあとに前の Run の配達が
                // 終わったとき、**同じ番号の別のタスク**の指示を完了に
                // してしまう (積んだ仕事は Run の切り替えでは消えない)。
                let mut job = crate::submit::Job::deferred(session, text, true);
                job.tag = panel::with_panel(|p| p.delivery_tag(&key));
                self.queue_submit(job)
            };
            panel::with_panel(|p| match (&blocked, queued) {
                (Some(why), _) => {
                    // **成功として記録しない。** 1 行も送っていないので、
                    // 冪等キーを完了にすると、人が手当てして Retry した
                    // あとも同じ鍵が抑止されて指示が二度と届かない。
                    p.ack_failed(&key);
                    p.note_instruction_blocked(task, why);
                }
                // **積めた = 届いた、ではない。** ここでは何も返さない
                // (Effect は「発行済み」のまま = 二重には出ない)。届いたか
                // 消えたかは `team_note_delivery` が 1 回だけ返す。
                (None, true) => {}
                (None, false) => p.ack_failed(&key),
            });
        }

        // ── 停止 (承認済みのものだけがここへ来る) ──
        let stops = panel::with_panel(|p| p.take_stops());
        for (key, session) in stops {
            if let Some(i) = self.agents.sessions.iter().position(|s| s.id == session) {
                self.close_agent(i);
            }
            // 相手が既に居なくても目的は果たされている (止まっている)。
            panel::with_panel(|p| p.ack_done(&key));
        }

        // ── 検証コマンドの実行 ──
        //
        // **UI スレッドでブロッキングしない。** 裏のスレッドで走らせて、
        // 終わったら結果だけを戻す。ACK は結果を戻せたときに返す
        // (`note_validation` が次の検証を発行できるよう記録を外す)。
        let validations = panel::with_panel(|p| p.take_validations());
        for (key, v) in validations {
            self.team_spawn_validation(key, v);
        }
        panel::with_panel(|p| p.collect_validations());
    }

    /// **配達の結末を Team へ返す** (`submit_tick` から 1 回だけ)。
    ///
    /// 積めたことを完了として記録すると、そのあと相手が消えても・入力欄が
    /// 空かないまま上限に達しても、Runtime は「指示は届いた」と信じたまま
    /// タスクを抱え続ける (冪等キーが完了なので二度と出し直されない)。
    pub(crate) fn team_note_delivery(&mut self, outcomes: Vec<(String, bool)>) {
        for (key, delivered) in outcomes {
            let task = panel::with_panel(|p| p.note_delivery(&key, delivered));
            if let Some(t) = task {
                self.toast(
                    trf(
                        "team.err.instruction_undelivered",
                        &[("task", t.to_string())],
                    ),
                    false,
                );
            }
        }
    }

    /// セッションの**再起動をまたぐ目印** (生ログの絶対パス)。
    ///
    /// 復元は同じログファイルへ書き戻す (`session::AgentSessionRec::log_file`)
    /// ので、この綴りは次の起動でも変わらない。
    fn team_session_identity(&self, session: SessionId) -> Option<String> {
        self.agents
            .sessions
            .iter()
            .find(|s| s.id == session)
            .and_then(|s| s.log_path.as_ref())
            .map(|p| p.to_string_lossy().into_owned())
    }

    /// **この起動要求に対応する、既に生きているセッションを探す。**
    ///
    /// 見つかれば `(セッション ID, 目印)`。優先順位:
    ///
    /// 1. **目印が一致するもの** — 前に起こしたセッションそのもの。
    ///    生ログのパスは復元をまたいで変わらないので、これが本命
    /// 2. 同じ作業フォルダで同じタブ名のもの — 起動には成功したが、
    ///    目印を残す前に落ちた窓の受け皿 (タブ名は Team が付けて復元される)
    ///
    /// **既に別の担当へ結び付いているセッションは選ばない。** 選ぶと 2 体の
    /// エージェントが同じ端末を共有し、指示が混ざる。
    fn team_adopt_session(
        &self,
        spec: &crate::features::team::imp::runtime::AgentLaunchSpec,
    ) -> Option<(SessionId, String)> {
        let bound = panel::with_panel(|p| p.bound_sessions());
        // **判断の規則は Team 側の純関数 1 本** (`launch::adopt_choice`)。
        // ここは事実を集めて渡すだけ — 規則を 2 か所に書かない。
        let facts: Vec<launch::SessionFact> = self
            .agents
            .sessions
            .iter()
            .map(|s| launch::SessionFact {
                id: s.id,
                identity: self.team_session_identity(s.id).unwrap_or_default(),
                title: s.title.clone(),
                cwd: s.cwd.clone(),
                running: s.running(),
                bound: bound.contains(&s.id),
            })
            .collect();
        let id = launch::adopt_choice(
            spec.adopt.as_deref(),
            &spec.name,
            &spec.workspace_root,
            &facts,
        )?;
        Some((id, self.team_session_identity(id)?))
    }

    /// エージェントを 1 体起こす。戻りは新しいセッション ID。
    ///
    /// **cwd は要求 (`spec.workspace_root`) が決める。** 画面のいまの
    /// フォルダ (`agent_cwd()`) を見てはいけない — Run を作ったあとに
    /// 利用者がフォルダを選び直すと、Team が面倒を見ているのとは違う
    /// ところでエージェントが動き出す。
    fn team_launch_agent(
        &mut self,
        spec: &crate::features::team::imp::runtime::AgentLaunchSpec,
        ctx: &egui::Context,
    ) -> Option<SessionId> {
        // **役割に合うプリセットを選ぶ。** 判断は純関数 1 本
        // (`roles::preset_for_role`) — 素のシェルではチームにならないので、
        // AI CLI として使えるものだけが候補になる。
        //
        // **この版では役割ごとのプリセットを持たないので、実際には全員が
        // 同じプリセットで動く。** 差し替え点をここ 1 か所に残してある。
        let table: Vec<(String, bool)> = self
            .cfg
            .agents
            .iter()
            .map(|p| {
                (
                    p.name.clone(),
                    crate::agents::spec_for_command(&p.command).is_some(),
                )
            })
            .collect();
        let idx = crate::features::team::imp::roles::preset_for_role(&table, spec.role)?;
        let command = self.cfg.agents.get(idx)?.command.clone();
        let before: std::collections::HashSet<SessionId> =
            self.agents.sessions.iter().map(|s| s.id).collect();
        self.launch_preset_with(idx, command, &spec.workspace_root, ctx);
        let session = self
            .agents
            .sessions
            .iter()
            .map(|s| s.id)
            .find(|id| !before.contains(id))?;
        // **役割は指示文にも載る** (`prompt.rs`)。ここでは端末の名前として
        // 見えるようにして、画面と実体がずれないようにする。
        if let Some(t) = self.agents.sessions.iter_mut().find(|s| s.id == session) {
            t.title = spec.name.clone();
        }
        Some(session)
    }

    /// 検証を裏で走らせる。
    ///
    /// **UI スレッドで待たない。** 時間切れと停止の判断は実行器
    /// ([`launch::run_words`]) が持ち、こちらは結果を受け取るだけ。
    /// どの経路で終わっても、結果か失敗のどちらかが必ず Runtime へ戻る。
    fn team_spawn_validation(
        &mut self,
        key: String,
        v: crate::features::team::imp::runtime::ValidationSpec,
    ) {
        let (tx, rx) = std::sync::mpsc::channel::<(String, TaskId, Vec<ValidationRun>)>();
        let task = v.task;
        let execution = v.execution.clone();
        let cwd = v.cwd.clone();
        let cmds = v.commands.clone();
        let approved = v.approved.clone();
        let timeout = std::time::Duration::from_secs(v.timeout_secs.max(1));
        let cancel = launch::new_cancel_flag();
        let pid = launch::new_pid_slot();
        let worker_cancel = cancel.clone();
        let worker_pid = pid.clone();
        let worker_exec = execution.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("zai-team-validate-{task}"))
            .spawn(move || {
                // 並べ方の決まりごと (どこで打ち切るか) は実行器が持つ。
                let runs = launch::run_validation_list(
                    &cmds,
                    &approved,
                    &cwd,
                    timeout,
                    &worker_cancel,
                    &worker_pid,
                );
                let _ = tx.send((worker_exec, task, runs));
            });
        match spawned {
            Ok(_) => panel::with_panel(|p| {
                // 走らせ始めたので受け取った旨を返す。実測の結果は
                // `collect_validations` が別途 Runtime へ戻す。
                let Some(owner) = p.owner() else {
                    // Run が入れ替わった。**渡さない** (走らせ始めたものは
                    // 札が立っているので、次の刻みで自分から畳む)。
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    return;
                };
                p.watch_validation(panel::ValidationJob {
                    owner,
                    task,
                    execution: execution.clone(),
                    commands: v.commands.iter().map(|c| c.display()).collect(),
                    started_at: crate::features::team::imp::model::now_secs(),
                    timeout_secs: v.timeout_secs,
                    cancel,
                    pid,
                    rx,
                });
                p.ack_done(&key);
            }),
            Err(e) => {
                // スレッドを作れないなら「実行できなかった」として戻す
                // (黙って未実行のままにすると永久に待つ)。
                let runs = v
                    .commands
                    .iter()
                    .map(|c| {
                        ValidationRun::new(
                            c.display(),
                            126,
                            crate::features::team::imp::model::ValidationOutcome::SpawnFailed,
                        )
                    })
                    .collect();
                panel::with_panel(|p| {
                    // 走らせられなかった = 実行不可として記録する。
                    // 黙って未実行のままにすると永久に待つ。
                    p.note_validation_for(&execution, task, runs);
                    p.ack_failed(&key);
                });
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
                // **エージェントの選択は外す。** 残すと Inspector に
                // 「同じラベルで別のタスクを指す Retry / Reassign」が
                // 2 段並ぶ (どちらが効くのか読み手に分からない)。
                p.selected_agent = None;
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
        // **基準は画面のいまのフォルダではなく、Team が持っている workspace。**
        // 実行中の Run があると切り替えを断るので、この 2 つは食い違いうる。
        // `agent_cwd()` で SPEC を解決すると、Run の workspace と別の場所の
        // ファイルを読んで計画してしまう。
        let (ws, form) = panel::with_panel(|p| (p.workspace().to_path_buf(), p.form.clone()));
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
            // **秒だけで作らない。** 同じ秒に 2 回始めると ID が衝突し、
            // 前の Run の検証結果や承認が新しい Run の同じ番号のタスクへ
            // 当たりうる (`runtime::new_run_id`)。
            run_id: crate::features::team::imp::runtime::new_run_id(),
            spec_source: source.clone(),
            agent_count: form.agents,
            max_attempts: form.max_attempts,
            review_required: form.review_required,
        };
        let roles = form.roles.clone();
        let title = form.goal_name.clone();
        let r = panel::with_panel(|p| {
            let r = p.plan_with(&spec_text, &source, opts, roles, &title);
            if r.is_ok() {
                p.form.open = false;
                p.form.error.clear();
                p.tab = BoardTab::Organization;
            }
            r
        });
        match r {
            Ok(()) => {
                // **承認モードとコスト上限は既存の仕組みへ渡す。**
                // Team 専用の第 2 の真実を作らない — 起動時の承認判定も
                // 送信時のコスト遮断も、既存の 1 本がそのまま効く。
                self.set_team_guardrails(&form.approval_mode, form.cost_limit);
                self.toast(tr("team.toast.plan_preview"), true);
            }
            Err(e) => panel::with_panel(|p| p.form.error = e),
        }
    }

    /// フォームの承認モードとコスト上限を、**既存の設定**へ反映する。
    ///
    /// Team だけの承認判定やコスト判定を作らないのが要で、こうしておくと
    /// 「Cockpit では止まるのに Team では止まらない」が構造的に起こらない。
    fn set_team_guardrails(&mut self, approval_mode: &str, cost_limit: f32) {
        // 未知の値は `ask` へ倒す (読めない設定を「自動でよい」と読まない)。
        let mode = match approval_mode {
            "auto" | "agent" => approval_mode,
            _ => "ask",
        };
        // **両方を見る。** 片方だけで判定すると、`global_approval_mode`
        // だけが変わったときに保存されず、記憶と設定ファイルがずれる。
        let mut changed = self.cfg.approval_mode != mode || self.cfg.global_approval_mode != mode;
        self.cfg.approval_mode = mode.to_string();
        self.cfg.global_approval_mode = mode.to_string();
        // 0 は「上限なし」。既存の cost_limit_session と同じ意味。
        let limit = cost_limit.max(0.0);
        if (self.cfg.cost_limit_session - limit).abs() > f32::EPSILON {
            self.cfg.cost_limit_session = limit;
            changed = true;
        }
        if changed {
            // 既存の保存経路をそのまま使う (Team だけ別の書き方をしない)。
            crate::config::save_state(&self.cfg);
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
