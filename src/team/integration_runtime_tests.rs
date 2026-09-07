//! 実フォルダと制御可能なセッションで、配送から最終反映までを確認する。
use super::super::{
    integration, outbox,
    planner::{PlanInput, StaticPlanner, TeamPlanner, IMPLEMENTATION_ONLY},
    task_workspace,
};
use super::*;

struct Harness {
    rt: TeamRuntime,
    sessions: Vec<SessionObs>,
    now: u64,
    next_session: SessionId,
}

impl Harness {
    fn new(root: &std::path::Path, agents: usize) -> Self {
        test_hooks::clear();
        let plan = StaticPlanner.plan(PlanInput {
            spec: format!("{IMPLEMENTATION_ONLY}\n成果物を作成する\n- 本文を作成する (files: body.txt)\n- 補足を作成する (files: notes.txt)"),
            source: "直接入力".into(), agent_count: agents, review_required: false,
            workspace_root: root.to_owned(), roles: vec![TeamRole::Implementer],
        }).unwrap();
        task_workspace::prepare(root, &plan.tasks).unwrap();
        let mut rt = TeamRuntime::from_plan(
            plan,
            root.to_owned(),
            RunOptions {
                run_id: new_run_id(),
                spec_source: "直接入力".into(),
                agent_count: agents,
                agent_presets: vec![],
                max_attempts: 3,
                review_required: false,
                guardrails: Default::default(),
            },
        );
        rt.set_outbox(
            root.parent()
                .unwrap()
                .join(format!("outbox-{}", rt.run().run_id)),
        );
        rt.apply_action(TeamAction::Start);
        Self {
            rt,
            sessions: vec![],
            now: now_secs(),
            next_session: 1,
        }
    }

    fn pump(&mut self, state: SessionState) -> Vec<TaskId> {
        self.now += 1;
        for session in &mut self.sessions {
            session.state = state;
        }
        let effects = self.rt.tick(&Observation {
            now: self.now,
            sessions: self.sessions.clone(),
        });
        let mut assigned = vec![];
        for effect in effects {
            let effect_key = effect.key();
            match effect {
                TeamEffect::StartAgent(spec) => {
                    let id = self.next_session;
                    self.next_session += 1;
                    self.rt.bind_session(&spec.agent_id, id, None);
                    self.rt.note_effect_done(&effect_key);
                    self.sessions.push(SessionObs {
                        id,
                        title: "fake".into(),
                        provider: "fake".into(),
                        state: SessionState::Idle,
                        text: String::new(),
                    });
                }
                TeamEffect::SendInstruction {
                    task, session, key, ..
                } => {
                    self.rt
                        .handoff_integration_writer(task, session, Some(std::process::id()))
                        .unwrap();
                    self.rt.note_effect_done(&key);
                    assigned.push(task);
                }
                TeamEffect::RunValidation(_) => panic!("新規Runへ検証工程を戻してはいけない"),
                _ => {}
            }
        }
        assigned
    }

    fn candidate(&self, task: TaskId) -> (PathBuf, String) {
        let (path, prefix, _) =
            task_workspace::execution(self.rt.workspace(), &self.rt.task(task).unwrap().files)
                .unwrap()
                .unwrap();
        (path, prefix)
    }

    fn report(&self, task: TaskId, relative: &str) -> (AgentId, String) {
        let agent = self.rt.task(task).unwrap().assigned_agent.clone().unwrap();
        let (_, prefix) = self.candidate(task);
        let body = serde_json::json!({"task_id": task, "agent_id": agent.to_string(), "status": "completed", "summary": "担当成果物を保存した", "changed_files": [format!("{prefix}/{relative}")], "validation": [], "blockers": []}).to_string();
        (agent, body)
    }

    fn complete(&mut self, task: TaskId, relative: &str, bytes: &str) -> (AgentId, String) {
        let (path, _) = self.candidate(task);
        std::fs::write(path.join(relative), bytes).unwrap();
        let (agent, body) = self.report(task, relative);
        assert_eq!(
            self.rt
                .accept_outbox(&agent, outbox::Kind::Result, &body, self.now)
                .unwrap(),
            AcceptOutcome::Applied
        );
        (agent, body)
    }

    fn workers(&mut self) {
        for _ in 0..12 {
            if self
                .rt
                .tasks()
                .iter()
                .filter(|t| t.key != "assemble")
                .all(|t| t.state == TeamTaskState::Completed)
            {
                return;
            }
            for task in self.pump(SessionState::Idle) {
                assert_ne!(
                    self.rt.task(task).unwrap().key,
                    "assemble",
                    "実装完了前に統合を配らない"
                );
                self.complete(task, &format!("worker-{task}.txt"), "実装済み");
            }
        }
        panic!("実装担当が依存待ちで停止した: {:?}", self.rt.tasks());
    }

    fn assembly(&mut self) -> TaskId {
        self.workers();
        for _ in 0..4 {
            for task in self.pump(SessionState::Idle) {
                if self.rt.task(task).unwrap().key == "assemble" {
                    return task;
                }
            }
        }
        panic!("統合指示が配送されなかった: {:?}", self.rt.tasks());
    }
}

fn root() -> PathBuf {
    let base = crate::test_util::unique_temp_dir("zaivern-integration-runtime", "source");
    let root = base.join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("body.txt"), "元の本文").unwrap();
    root
}

fn locked(root: &std::path::Path) -> bool {
    integration::try_acquire(root, "observer")
        .unwrap()
        .is_none()
}

#[test]
fn 一体で実装から統合へ進み担当が停止するまで元フォルダを変更しない() {
    let root = root();
    let mut h = Harness::new(&root, 1);
    let task = h.assembly();
    assert_eq!(
        h.rt.agents()
            .iter()
            .filter(|a| a.kind == AgentKind::ManagedSession)
            .count(),
        1
    );
    assert!(locked(&root), "配送時点でRun間の所有権を持つ");
    h.complete(task, "body.txt", "最終本文");
    h.pump(SessionState::Working);
    assert_eq!(h.rt.task(task).unwrap().state, TeamTaskState::Validating);
    assert!(locked(&root));
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "元の本文"
    );
    h.pump(SessionState::Idle);
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "最終本文"
    );
    assert_eq!(h.rt.task(task).unwrap().state, TeamTaskState::Completed);
    assert_eq!(h.rt.goal().status, GoalStatus::Completed);
    assert!(!locked(&root));
}

#[test]
fn 同じ元フォルダの二つのrunへ統合を同時配送しない() {
    let root = root();
    let mut first = Harness::new(&root, 2);
    let mut second = Harness::new(&root, 2);
    let first_task = first.assembly();
    second.workers();
    assert!(second.pump(SessionState::Idle).is_empty());
    assert!(second
        .rt
        .tasks()
        .iter()
        .filter(|t| t.key == "assemble")
        .all(|t| t.assigned_agent.is_none()));
    first.complete(first_task, "first.txt", "一つ目");
    first.pump(SessionState::Working);
    assert!(second.pump(SessionState::Idle).is_empty());
    first.pump(SessionState::Idle);
    let second_task = second.assembly();
    second.complete(second_task, "second.txt", "二つ目");
    second.pump(SessionState::Idle);
    assert_eq!(
        std::fs::read_to_string(root.join("first.txt")).unwrap(),
        "一つ目"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("second.txt")).unwrap(),
        "二つ目"
    );
    assert_eq!(second.rt.goal().status, GoalStatus::Completed);
}

#[test]
fn 停止要求だけでは統合所有権を解放せず終了観測後に再開できる() {
    let root = root();
    let mut h = Harness::new(&root, 1);
    let task = h.assembly();
    h.complete(task, "body.txt", "停止した担当の候補");
    h.rt.apply_action(TeamAction::Stop);
    h.pump(SessionState::Working);
    assert!(locked(&root));
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "元の本文"
    );
    h.pump(SessionState::Exited);
    assert!(!locked(&root));
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "元の本文"
    );
    h.rt.apply_action(TeamAction::Resume);
    h.sessions.clear();
    let retried = h.assembly();
    assert_eq!(retried, task);
    h.complete(retried, "body.txt", "再開後の成果物");
    h.pump(SessionState::Idle);
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "再開後の成果物"
    );
    assert_eq!(h.rt.goal().status, GoalStatus::Completed);
}

#[test]
fn 不正重複遅延報告は統合を早期実行も再実行もしない() {
    let root = root();
    let mut h = Harness::new(&root, 1);
    let task = h.assembly();
    let (agent, _) = h.report(task, "body.txt");
    assert!(h
        .rt
        .accept_outbox(&agent, outbox::Kind::Result, "{", h.now)
        .is_err());
    assert!(locked(&root));
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "元の本文"
    );
    let (agent, body) = h.complete(task, "body.txt", "正式な成果物");
    assert_eq!(
        h.rt.accept_outbox(&agent, outbox::Kind::Result, &body, h.now)
            .unwrap(),
        AcceptOutcome::Duplicate
    );
    h.pump(SessionState::Working);
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "元の本文"
    );
    h.pump(SessionState::Idle);
    std::fs::write(root.join("body.txt"), "完了後のユーザー編集").unwrap();
    let _ =
        h.rt.accept_outbox(&agent, outbox::Kind::Result, &body, h.now);
    h.pump(SessionState::Idle);
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "完了後のユーザー編集"
    );
    assert!(!locked(&root));
}

#[test]
fn 隔離後のユーザー変更は保持して未解決ありとして提出する() {
    let root = root();
    let mut h = Harness::new(&root, 1);
    let task = h.assembly();
    h.complete(task, "body.txt", "古い本文からの変更");
    std::fs::write(root.join("body.txt"), "ユーザーの更新").unwrap();
    h.pump(SessionState::Idle);
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "ユーザーの更新"
    );
    assert_eq!(h.rt.task(task).unwrap().state, TeamTaskState::Submitted);
    assert_eq!(h.rt.goal().status, GoalStatus::Submitted);
    assert!(!h.rt.task(task).unwrap().blockers.is_empty());
    assert!(!locked(&root));
}

#[test]
fn 同じファイルを変更した二つのrunは後続を未解決として先行成果を保護する() {
    let root = root();
    let mut first = Harness::new(&root, 1);
    let mut second = Harness::new(&root, 1);
    let first_task = first.assembly();
    second.workers();
    first.complete(first_task, "body.txt", "先行Runの本文");
    first.pump(SessionState::Idle);
    let second_task = second.assembly();
    second.complete(second_task, "body.txt", "後続Runの古い候補");
    second.pump(SessionState::Idle);
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "先行Runの本文"
    );
    assert_eq!(first.rt.goal().status, GoalStatus::Completed);
    assert_eq!(second.rt.goal().status, GoalStatus::Submitted);
    assert!(!locked(&root));
}

#[test]
fn 一時停止中に受けた統合報告は所有権を保持し再開後だけ反映する() {
    let root = root();
    let mut h = Harness::new(&root, 1);
    let task = h.assembly();
    h.rt.apply_action(TeamAction::Pause);
    h.complete(task, "body.txt", "再開して公開する本文");
    h.pump(SessionState::Idle);
    assert!(locked(&root));
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "元の本文"
    );
    h.rt.apply_action(TeamAction::Resume);
    h.pump(SessionState::Idle);
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "再開して公開する本文"
    );
    assert_eq!(h.rt.goal().status, GoalStatus::Completed);
    assert!(!locked(&root));
}

#[test]
fn 停止と再配送で旧指示の本文と確定キーを拒否する() {
    let root = root();
    let mut h = Harness::new(&root, 1);
    let id = h.assembly();
    let task = h.rt.task(id).unwrap().clone();
    let session = task.assigned_session.unwrap();
    let key = instruction_key(
        id,
        task.assigned_agent.as_ref().unwrap(),
        task.attempts,
        task.dispatch_seq,
    );
    assert_eq!(h.rt.instruction_delivery_ready(&key, session), Some(true));
    h.rt.apply_action(TeamAction::Stop);
    assert_eq!(h.rt.instruction_delivery_ready(&key, session), Some(false));
    h.pump(SessionState::Idle);
    assert!(locked(&root), "停止未確認で未配送指示の所有権を解放した");
    h.pump(SessionState::Exited);
    assert_eq!(h.rt.instruction_delivery_ready(&key, session), None);
    h.rt.apply_action(TeamAction::Resume);
    h.sessions.clear();
    h.assembly();
    assert_eq!(h.rt.instruction_delivery_ready(&key, session), None);
    let current = h.rt.task(id).unwrap();
    let next = instruction_key(
        id,
        current.assigned_agent.as_ref().unwrap(),
        current.attempts,
        current.dispatch_seq,
    );
    assert_ne!(key, next);
    assert_eq!(
        h.rt.instruction_delivery_ready(&next, current.assigned_session.unwrap()),
        Some(true)
    );
}

#[test]
fn 未解決の統合候補は停止確認後に提出しても成功や承認を偽装しない() {
    let root = root();
    let mut h = Harness::new(&root, 1);
    let task = h.assembly();
    let (candidate, _) = h.candidate(task);
    std::fs::write(candidate.join("body.txt"), "未解決のある提出物").unwrap();
    h.rt.submit_with_issues(task, "内容の一部が未解決");
    h.pump(SessionState::Working);
    assert!(locked(&root));
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "元の本文"
    );
    h.pump(SessionState::Idle);
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "未解決のある提出物"
    );
    let task = h.rt.task(task).unwrap();
    assert_eq!(task.state, TeamTaskState::Submitted);
    assert_eq!(h.rt.goal().status, GoalStatus::Submitted);
    assert!(task
        .blockers
        .iter()
        .any(|s| s.contains("内容の一部が未解決")));
    assert!(!task.review.approved());
    assert!(!locked(&root));
}

#[test]
fn 完了報告後に復元しても旧担当の停止と新たな統合所有権が必要() {
    let root = root();
    let mut original = Harness::new(&root, 1);
    let task = original.assembly();
    original.complete(task, "body.txt", "復元前の候補");
    let saved = original.rt.to_saved();
    assert_eq!(
        original.rt.task(task).unwrap().state,
        TeamTaskState::Validating
    );
    let restored = TeamRuntime::restore(saved, root.clone());
    assert_eq!(restored.task(task).unwrap().state, TeamTaskState::Ready);
    let mut h = Harness {
        rt: restored,
        sessions: vec![],
        now: now_secs(),
        next_session: 100,
    };
    for _ in 0..3 {
        assert!(
            h.pump(SessionState::Idle).is_empty(),
            "旧担当が生きている間に再配送した"
        );
    }
    assert!(locked(&root));
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "元の本文"
    );
    original.rt.apply_action(TeamAction::Stop);
    original.pump(SessionState::Exited);
    assert!(!locked(&root));
    assert_eq!(h.assembly(), task);
    assert!(locked(&root));
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "元の本文"
    );
    h.complete(task, "body.txt", "復元後の正式な候補");
    h.pump(SessionState::Idle);
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "復元後の正式な候補"
    );
    assert_eq!(h.rt.goal().status, GoalStatus::Completed);
}

#[test]
fn 旧形式の直接統合は復元時に成果と履歴を保持して自動反映しない() {
    let root = root();
    let mut original = Harness::new(&root, 1);
    original.workers();
    let mut saved = original.rt.to_saved();
    let task = saved
        .tasks
        .iter_mut()
        .find(|t| t.key == "assemble")
        .unwrap();
    let id = task.id;
    task.files = vec!["**".into()];
    task.last_summary = "旧実装の引継ぎ情報".into();
    let history: Vec<_> = saved.events.iter().map(|e| e.id).collect();
    let restored = TeamRuntime::restore(saved, root.clone());
    assert_eq!(restored.task(id).unwrap().state, TeamTaskState::Submitted);
    assert!(restored
        .task(id)
        .unwrap()
        .context
        .iter()
        .any(|line| line.contains("旧実装の引継ぎ情報")));
    assert!(history
        .iter()
        .all(|id| restored.events().any(|e| e.id == *id)));
    let mut h = Harness {
        rt: restored,
        sessions: vec![],
        now: now_secs(),
        next_session: 100,
    };
    for _ in 0..3 {
        assert!(h.pump(SessionState::Idle).is_empty());
    }
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "元の本文"
    );
    assert_eq!(h.rt.goal().status, GoalStatus::Submitted);
    assert!(!locked(&root));
}

#[test]
fn 実装担当の復元でも旧担当の終了確認まで同じ隔離先へ再配送しない() {
    let root = root();
    let mut original = Harness::new(&root, 1);
    let task = (0..4)
        .find_map(|_| original.pump(SessionState::Idle).first().copied())
        .expect("実装の配送");
    assert_ne!(original.rt.task(task).unwrap().key, "assemble");
    let (candidate, _) = original.candidate(task);
    std::fs::write(candidate.join("body.txt"), "旧担当が作業中の本文").unwrap();
    let restored = TeamRuntime::restore(original.rt.to_saved(), root.clone());
    let mut h = Harness {
        rt: restored,
        sessions: vec![],
        now: now_secs(),
        next_session: 100,
    };
    for _ in 0..3 {
        assert!(
            h.pump(SessionState::Idle).is_empty(),
            "生きている実装担当の隔離先へ再配送した"
        );
    }
    original.rt.apply_action(TeamAction::Stop);
    original.pump(SessionState::Working);
    assert!(
        h.pump(SessionState::Idle).is_empty(),
        "停止要求だけで旧実装担当の所有権を解放した"
    );
    assert_eq!(
        std::fs::read_to_string(candidate.join("body.txt")).unwrap(),
        "旧担当が作業中の本文"
    );
    original.pump(SessionState::Exited);
    let reassigned = (0..4)
        .find_map(|_| h.pump(SessionState::Idle).first().copied())
        .expect("旧担当終了後の再配送");
    assert_eq!(reassigned, task);
    assert_eq!(h.candidate(reassigned).0, candidate);
    h.complete(reassigned, "body.txt", "再開後の実装");
    let assembly = h.assembly();
    h.complete(assembly, "body.txt", "再開後の実装");
    h.pump(SessionState::Idle);
    assert_eq!(
        std::fs::read_to_string(root.join("body.txt")).unwrap(),
        "再開後の実装"
    );
    assert_eq!(h.rt.goal().status, GoalStatus::Completed);
}

#[test]
fn 実装担当が完了報告しても書込み中は統合を配送しない() {
    for stopped in [SessionState::Idle, SessionState::Exited] {
        let root = root();
        let mut h = Harness::new(&root, 1);
        let task = (0..4)
            .find_map(|_| h.pump(SessionState::Idle).first().copied())
            .expect("実装の配送");
        assert_ne!(h.rt.task(task).unwrap().key, "assemble");
        h.complete(task, "body.txt", "完了報告済みの本文");
        for _ in 0..3 {
            assert!(
                h.pump(SessionState::Working).is_empty(),
                "旧実装担当が書込み中なのに後続を配送した"
            );
        }
        assert_eq!(
            std::fs::read_to_string(root.join("body.txt")).unwrap(),
            "元の本文"
        );
        let effects = h.pump(stopped);
        let assembly = effects
            .into_iter()
            .find(|id| h.rt.task(*id).unwrap().key == "assemble")
            .unwrap_or_else(|| h.assembly());
        assert!(h.rt.task(assembly).unwrap().dependencies.contains(&task));
        h.complete(assembly, "body.txt", "完了報告済みの本文");
        h.pump(SessionState::Idle);
        assert_eq!(
            std::fs::read_to_string(root.join("body.txt")).unwrap(),
            "完了報告済みの本文"
        );
        assert_eq!(h.rt.goal().status, GoalStatus::Completed);
    }
}
