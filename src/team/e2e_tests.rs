//! **受入シナリオの E2E** — 偽エージェントで全過程を自動実行する。
//!
//! ネットワークも実際の CLI も要らない。要求どおりの筋書きを 1 本通し、
//! 途中で Organization Board / Task Kanban / Timeline が**同じ Runtime の
//! 状態**を見ていることまで確かめる (画面ごとに別の真実を持たない)。
//!
//! ```text
//! Goal 作成 → SPEC 解析 → Team Plan 生成 → Team Lead 表示
//! → Backend / QA チーム表示 → Task A と Task B が Ready
//! → 2 つの ManagedSession へ並列割当 → 子 Agent が Board へ表示
//! → Task A 実装完了 → validation 成功 → Reviewer へ割当 → APPROVE → 完了
//! → Task B 実装完了 → validation 成功 → REQUEST_CHANGES → 差し戻し
//! → 指摘を context へ追加 → 再割当 → validation 成功 → APPROVE → 完了
//! → Integrator 起動 → fmt / build / test 成功 → Goal Completed
//! ```

use std::path::PathBuf;

use crate::coordinator::SessionState;

use super::model::*;
use super::planner::{PlanInput, StaticPlanner, TeamPlanner};
use super::result_parser as rp;
use super::reviewer;
use super::runtime::*;
use super::view_model;

const SPEC: &str = "\
# 認証機能を実装する

## 要件
- ログイン API を実装する (src/auth/login.rs)
- トークン更新 API を実装する (src/auth/refresh.rs)

## 完了条件
- 認証 API が動作する
- テストが成功する
- レビューが承認される

## 検証
- cargo test auth
";

/// 偽のチーム。全セッションを常に Idle として観測し、狙った 1 体にだけ
/// 文字列を見せる。
struct Lab {
    rt: TeamRuntime,
    sessions: Vec<SessionId>,
    next_session: SessionId,
    now: u64,
    /// 検証の実行結果 (偽の実行器)。**既定は成功**だが、ここを偽にすると
    /// 「エージェントは成功と言ったが実際は落ちた」を再現できる。
    validation_passes: bool,
    /// 検証の実行許可を人が承認するか。**既定は承認する**。
    ///
    /// 偽にすると「承認していないので 1 行も走らない」を再現できる。
    /// 実物でも `cargo test` はリポジトリ内のコードを実行するので、
    /// 人が承認するまで走らない。
    approve_validation: bool,
    /// 実行を頼まれた検証。**次の tick で返す** — 実物も裏のスレッドで
    /// 走らせて、終わったフレームで結果を戻す (同じ tick で即答すると、
    /// 「検証待ち」という状態が 1 度も観測されない筋書きになる)。
    queued_validations: Vec<super::runtime::ValidationSpec>,
}

impl Lab {
    fn new(agents: usize) -> Self {
        let plan = StaticPlanner
            .plan(PlanInput {
                spec: SPEC.to_string(),
                source: "SPEC.md".into(),
                agent_count: agents,
                review_required: true,
                roles: Vec::new(),
            })
            .expect("計画できるべき");
        let mut rt = TeamRuntime::from_plan(
            plan,
            PathBuf::from("/zaivern-team-e2e"),
            RunOptions {
                run_id: "run-e2e".into(),
                spec_source: "SPEC.md".into(),
                agent_count: agents,
                max_attempts: 3,
                review_required: true,
            },
        );
        rt.apply_action(TeamAction::Start);
        Self {
            rt,
            sessions: Vec::new(),
            next_session: 1,
            now: 100,
            validation_passes: true,
            approve_validation: true,
            queued_validations: Vec::new(),
        }
    }

    /// 1 tick 回す。`target` に指定したセッションにだけテキストを見せる。
    fn tick(&mut self, target: Option<SessionId>, text: &str) -> Vec<TeamEffect> {
        self.now += 1;
        // 前の tick で頼まれた検証の結果を、いま戻す (裏のスレッドの模擬)。
        let code = i32::from(!self.validation_passes);
        for v in std::mem::take(&mut self.queued_validations) {
            let runs = v
                .commands
                .iter()
                .map(|c| {
                    ValidationRun::new(
                        c,
                        code,
                        if code == 0 {
                            ValidationOutcome::Passed
                        } else {
                            ValidationOutcome::Failed
                        },
                    )
                })
                .collect();
            self.rt.note_validation_for(&v.execution, v.task, runs);
        }
        let rows: Vec<SessionObs> = self
            .sessions
            .iter()
            .map(|s| SessionObs {
                id: *s,
                title: format!("agent{s}"),
                provider: "claude".into(),
                state: SessionState::Idle,
                text: if Some(*s) == target {
                    text.to_string()
                } else {
                    String::new()
                },
            })
            .collect();
        let eff = self.rt.tick(&Observation {
            now: self.now,
            sessions: rows,
        });
        // **要求には必ず応える。** 応えないと「発行したのに誰も実行しない」
        // 状態になり、テストが実物と違う筋書きを回すことになる。
        //
        // 実行側は成功したら ACK を返す。返さない限り Runtime は「済んだ」
        // と見なさないので、ここでの ACK は実物の app と同じ責務。
        let mut acks: Vec<String> = Vec::new();
        let mut validations: Vec<super::runtime::ValidationSpec> = Vec::new();
        for e in &eff {
            match e {
                TeamEffect::StartAgent(s) => {
                    let sid = self.next_session;
                    self.next_session += 1;
                    self.rt.bind_session(&s.agent_id, sid);
                    self.sessions.push(sid);
                    acks.push(e.key());
                }
                TeamEffect::RunValidation(v) => {
                    // 受け取ったので ACK。結果は次の tick で返す。
                    validations.push(v.clone());
                    acks.push(e.key());
                }
                TeamEffect::PersistState => {}
                _ => acks.push(e.key()),
            }
        }
        for k in acks {
            if !k.is_empty() {
                self.rt.note_effect_done(&k);
            }
        }
        // **検証は Zaivern が走らせる。** エージェントの自己申告ではない。
        self.queued_validations.extend(validations);
        // 人の承認 (実物では画面のボタン)。承認しない限り検証は発行されない。
        if self.approve_validation {
            let ids: Vec<u64> = self
                .rt
                .decisions()
                .iter()
                .filter(|d| d.kind == DecisionKind::ValidationExecution)
                .map(|d| d.id)
                .collect();
            for id in ids {
                self.rt.apply_action(TeamAction::ApproveDecision(id));
            }
        }
        eff
    }

    fn idle(&mut self) -> Vec<TeamEffect> {
        self.tick(None, "")
    }

    /// いま実装中のタスク (session, task, agent)。
    fn working(&self) -> Vec<(SessionId, TaskId, String)> {
        let mut v: Vec<(SessionId, TaskId, String)> = self
            .rt
            .tasks()
            .iter()
            .filter(|t| t.state.is_working() && t.review_of.is_none())
            .filter_map(|t| {
                Some((
                    t.assigned_session?,
                    t.id,
                    t.assigned_agent.as_ref()?.0.clone(),
                ))
            })
            .collect();
        v.sort();
        v
    }

    /// タスク `tid` の完了を報告する。
    fn report_done(&mut self, tid: TaskId) {
        let t = self.rt.task(tid).expect("タスク").clone();
        let sid = t.assigned_session.expect("担当セッション");
        let agent = t.assigned_agent.clone().expect("担当").0;
        let v: Vec<String> = t
            .validation_commands
            .iter()
            .map(|c| format!("{{\"command\":\"{c}\",\"exit_code\":0}}"))
            .collect();
        let f: Vec<String> = t.files.iter().map(|x| format!("\"{x}\"")).collect();
        let block = format!(
            "{open}\n{{\"task_id\":{tid},\"agent_id\":\"{agent}\",\"status\":\"completed\",\
             \"summary\":\"実装しました\",\"changed_files\":[{files}],\"validation\":[{v}],\"blockers\":[]}}\n{close}",
            open = rp::RESULT_OPEN,
            close = rp::RESULT_CLOSE,
            files = f.join(","),
            v = v.join(","),
        );
        self.tick(Some(sid), &block);
    }

    /// タスク `tid` のレビュー結果を報告する。
    fn report_review(&mut self, tid: TaskId, approve: bool) {
        let rev = self
            .rt
            .tasks()
            .iter()
            .find(|t| t.review_of == Some(tid) && t.state.is_working())
            .cloned()
            .unwrap_or_else(|| {
                // 落ちたときに追える情報を出す (「なぜか配られない」で
                // 時間を溶かさないため)。
                panic!(
                    "#{tid} のレビュー担当がいない。tasks={:?} agents={:?}",
                    self.rt
                        .tasks()
                        .iter()
                        .map(|t| (t.id, t.state, t.review_of, t.assigned_session))
                        .collect::<Vec<_>>(),
                    self.rt
                        .agents()
                        .iter()
                        .map(|a| (a.id.0.clone(), a.session_id, a.state))
                        .collect::<Vec<_>>(),
                )
            });
        let sid = rev.assigned_session.expect("レビュー担当セッション");
        let block = if approve {
            format!(
                "{open}\n{{\"task_id\":{tid},\"verdict\":\"APPROVE\",\"findings\":[],\
                 \"summary\":\"問題なし\"}}\n{close}",
                open = reviewer::REVIEW_OPEN,
                close = reviewer::REVIEW_CLOSE
            )
        } else {
            format!(
                "{open}\n{{\"task_id\":{tid},\"verdict\":\"REQUEST_CHANGES\",\
                 \"findings\":[\"異常系のテストが足りない\"],\"summary\":\"要修正\"}}\n{close}",
                open = reviewer::REVIEW_OPEN,
                close = reviewer::REVIEW_CLOSE
            )
        };
        self.tick(Some(sid), &block);
    }

    /// 親エージェントがサブエージェントを報告する。
    fn report_sub_agent(&mut self, parent_session: SessionId, sub: &str, role: &str) {
        let parent = self
            .rt
            .agents()
            .iter()
            .find(|a| a.session_id == Some(parent_session))
            .map(|a| a.id.0.clone())
            .expect("親エージェント");
        let block = format!(
            "{open}\n{{\"kind\":\"sub_agent_started\",\"agent_id\":\"{sub}\",\
             \"parent_id\":\"{parent}\",\"role\":\"{role}\",\"action\":\"作業中\"}}\n{close}",
            open = rp::EVENT_OPEN,
            close = rp::EVENT_CLOSE
        );
        self.tick(Some(parent_session), &block);
    }

    fn snapshot(&self) -> view_model::TeamSnapshot {
        view_model::snapshot(&self.rt, self.now)
    }
}

#[test]
fn 受入シナリオを最後まで通す() {
    let mut lab = Lab::new(4);

    // ── 計画 ──
    assert_eq!(lab.rt.goal().title, "認証機能を実装する");
    assert_eq!(lab.rt.goal().definition_of_done.len(), 3);
    // Team Lead が居る
    let lead = lab
        .rt
        .agents()
        .iter()
        .find(|a| a.role == TeamRole::TeamLead)
        .expect("Team Lead が居るべき");
    assert_eq!(lead.parent_id, None);
    assert_eq!(lead.kind, AgentKind::ManagedSession);
    // 専門チームのレーンが立っている
    let lanes: Vec<&str> = lab.rt.teams().iter().map(|t| t.name.as_str()).collect();
    assert!(lanes.contains(&"Implementation"), "{lanes:?}");
    assert!(lanes.contains(&"QA & Review"), "{lanes:?}");
    assert!(lanes.contains(&"Integration"), "{lanes:?}");
    assert!(lanes.len() <= view_model::MAX_LANES_ON_SCREEN);

    // ── 起動と並列割り当て ──
    lab.idle(); // StartAgent → bind
    lab.idle(); // 割り当て
    let work = lab.working();
    assert_eq!(work.len(), 2, "Task A と Task B が並列に走るべき: {work:?}");
    let (_, task_a, _) = work[0].clone();
    let (sid_b, task_b, _) = work[1].clone();
    assert_ne!(work[0].0, sid_b, "別々のセッションへ配るべき");

    // ── 子エージェントの報告が Board に出る ──
    lab.report_sub_agent(sid_b, "backend-test-1", "tester");
    let sub = lab
        .rt
        .agent(&AgentId::new("backend-test-1"))
        .expect("子エージェントが Board に出るべき");
    assert_eq!(sub.kind, AgentKind::ReportedSubAgent);
    assert!(
        !sub.can_open_terminal(),
        "端末を開けると嘘をついてはいけない"
    );

    // ── Task A: 完了 → 検証 → レビュー → APPROVE → 完了 ──
    lab.report_done(task_a);
    // **報告だけでは進まない。** Zaivern 自身の検証を待つ。
    assert_eq!(
        lab.rt.task(task_a).unwrap().state,
        TeamTaskState::Validating,
        "報告だけでレビューへ進めてはいけない"
    );
    // 承認 → 発行 → 実測が戻る、で 2 tick。
    lab.idle();
    lab.idle();
    assert_eq!(
        lab.rt.task(task_a).unwrap().state,
        TeamTaskState::Reviewing,
        "実測が通ったのにレビューへ進まない"
    );
    assert!(lab
        .rt
        .task(task_a)
        .unwrap()
        .validation
        .passed(&lab.rt.task(task_a).unwrap().validation_commands));
    lab.idle(); // レビュー担当へ割り当て
    let rev = lab
        .rt
        .tasks()
        .iter()
        .find(|t| t.review_of == Some(task_a))
        .expect("レビュータスク");
    assert_ne!(
        rev.assigned_session,
        lab.rt.task(task_a).unwrap().assigned_session,
        "実装した本人がレビューしている"
    );
    lab.report_review(task_a, true);
    assert_eq!(lab.rt.task(task_a).unwrap().state, TeamTaskState::Completed);

    // ── Task B: 完了 → REQUEST_CHANGES → 差し戻し → 再実装 → APPROVE ──
    lab.report_done(task_b);
    lab.idle();
    lab.report_review(task_b, false);
    let b = lab.rt.task(task_b).unwrap();
    // 差し戻され、同じ tick で再び配られる (Ready を経て Running)。
    // **完了になっていないこと**が守りたい性質。
    assert!(
        matches!(b.state, TeamTaskState::Ready | TeamTaskState::Running),
        "差し戻されていない: {:?}",
        b.state
    );
    assert!(
        b.context.iter().any(|c| c.contains("異常系")),
        "指摘が context へ載っていない: {:?}",
        b.context
    );
    assert_eq!(b.attempts, 1);
    lab.idle(); // 再割り当て
    assert!(
        lab.working().iter().any(|(_, t, _)| *t == task_b),
        "再割り当てされていない"
    );
    lab.report_done(task_b);
    lab.idle();
    lab.report_review(task_b, true);
    assert_eq!(lab.rt.task(task_b).unwrap().state, TeamTaskState::Completed);

    // ── 統合タスクが Ready になり、割り当てられる ──
    lab.idle();
    let integ = lab
        .rt
        .tasks()
        .iter()
        .find(|t| t.role == TeamRole::Integrator)
        .cloned()
        .expect("統合タスク");
    assert!(
        integ.state != TeamTaskState::Pending,
        "依存が済んだのに Ready にならない: {:?}",
        integ.state
    );
    lab.idle();
    assert!(
        lab.working().iter().any(|(_, t, _)| *t == integ.id),
        "統合が割り当てられていない: {:?}",
        lab.working()
    );

    // ── 統合完了 → レビュー → Goal Completed ──
    lab.report_done(integ.id);
    lab.idle();
    lab.report_review(integ.id, true);
    lab.idle();

    assert_eq!(
        lab.rt.goal().status,
        GoalStatus::Completed,
        "Goal が完了していない。タスク: {:?}",
        lab.rt
            .tasks()
            .iter()
            .map(|t| (t.id, t.state))
            .collect::<Vec<_>>()
    );

    // ── 3 つの画面が同じ状態を見ている ──
    let s = lab.snapshot();
    assert_eq!(s.goal.status, GoalStatus::Completed);
    assert_eq!(s.metrics.progress_pct, 100);
    assert_eq!(s.metrics.tasks_done, s.metrics.tasks_total);
    // Organization: 親子関係
    assert!(s.agents.iter().any(|a| a.role == TeamRole::TeamLead));
    assert!(s
        .agents
        .iter()
        .any(|a| a.kind == AgentKind::ReportedSubAgent && !a.can_open_terminal));
    // Tasks: Kanban の元データ
    assert!(s.tasks.iter().all(|t| t.state == TeamTaskState::Completed));
    // Timeline: 出来事が残っている
    assert!(s
        .events
        .iter()
        .any(|e| e.kind == TeamEventKind::GoalCompleted));
    assert!(s.events.len() <= s.feed_cap);
    // Current Action Bar
    assert_eq!(view_model::current_action(&s).text, "Goal Completed");
    // フェーズは Task Graph から出ている
    assert!(s
        .phases
        .iter()
        .all(|(_, st)| *st == super::graph::PhaseStatus::Done));
}

#[test]
fn 六十四体でも画面のデータが破綻しない() {
    // 大量のエージェントとイベントを積んでも、スナップショットは上限を守る。
    let mut lab = Lab::new(64);
    lab.idle();
    lab.idle();
    let parent = lab.sessions[0];
    for i in 0..80 {
        lab.report_sub_agent(parent, &format!("sub-{i}"), "tester");
    }
    let s = lab.snapshot();
    assert!(s.events.len() <= s.feed_cap, "{}", s.events.len());
    assert!(
        s.agents.len() >= 80,
        "サブエージェントが登録されていない: {}",
        s.agents.len()
    );
    // どのエージェントも「開けない端末」を主張しない
    for a in &s.agents {
        if a.kind == AgentKind::ReportedSubAgent {
            assert!(!a.can_open_terminal);
        }
    }
    // レイアウトは破綻しない
    let l = view_model::lane_layout(1480.0, s.teams.len());
    assert!(l.visible >= 1);
}

#[test]
fn 既存coordinatorの重なり判定を迂回しない() {
    // 同じファイルを担当する 2 タスクは、同時に走らない。
    let mut lab = Lab::new(4);
    lab.idle();
    lab.idle();
    let work = lab.working();
    let mut seen: Vec<String> = Vec::new();
    for (_, tid, _) in &work {
        for f in &lab.rt.task(*tid).unwrap().files {
            for s in &seen {
                assert!(
                    !crate::lease::overlaps(s, f),
                    "重なる担当を同時に配った: {s} / {f}"
                );
            }
            seen.push(f.clone());
        }
    }
}

#[test]
fn 承認しなければ検証は一行も走らずgoalは完了しない() {
    // **`cargo test` はリポジトリ内の任意コードを実行できる。** sandbox を
    // 持たない以上、人が承認するまで 1 行も走らせない。承認しなければ
    // Goal も完了しない (自動で先へ進む抜け道が無い)。
    let mut lab = Lab::new(4);
    lab.approve_validation = false;
    lab.idle();
    lab.idle();
    let work = lab.working();
    assert!(!work.is_empty());
    let (_, tid, _) = work[0].clone();
    lab.report_done(tid);
    for _ in 0..6 {
        let eff = lab.idle();
        assert!(
            !eff.iter()
                .any(|e| matches!(e, TeamEffect::RunValidation(_))),
            "承認していないのに検証を実行した: {eff:?}"
        );
    }
    assert_eq!(lab.rt.task(tid).unwrap().state, TeamTaskState::Validating);
    assert!(
        lab.rt
            .decisions()
            .iter()
            .any(|d| d.kind == DecisionKind::ValidationExecution),
        "承認を求めていない"
    );
    assert_ne!(lab.rt.goal().status, GoalStatus::Completed);
}

#[test]
fn 自己申告が嘘でも実測が通らなければgoalは完了しない() {
    // エージェントは「`cargo test auth` は成功した」と報告するが、
    // **Zaivern が実際に走らせると落ちる**という筋書き。
    let mut lab = Lab::new(4);
    lab.validation_passes = false;
    lab.idle();
    lab.idle();
    let work = lab.working();
    assert!(!work.is_empty());
    let (_, tid, _) = work[0].clone();
    lab.report_done(tid);
    assert_eq!(
        lab.rt.task(tid).unwrap().state,
        TeamTaskState::Validating,
        "報告だけで先へ進んだ"
    );
    lab.idle(); // RunValidation → 次の tick で失敗が返る
    lab.idle();
    let t = lab.rt.task(tid).unwrap();
    assert_ne!(t.state, TeamTaskState::Reviewing, "実測が落ちたのにレビューへ進んだ");
    assert_ne!(t.state, TeamTaskState::Completed, "実測が落ちたのに完了した");
    assert!(
        !lab.rt.tasks().iter().any(|x| x.review_of == Some(tid)),
        "実測が落ちたのにレビュータスクを作った"
    );
    // 食い違いは次の担当へ伝わる
    assert!(
        lab.rt
            .task(tid)
            .unwrap()
            .context
            .iter()
            .any(|c| c.contains("実際には失敗")),
        "申告と実測の食い違いを次の担当へ渡していない: {:?}",
        lab.rt.task(tid).unwrap().context
    );
    assert_ne!(lab.rt.goal().status, GoalStatus::Completed);
}
