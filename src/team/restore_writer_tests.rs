//! 復元状態と永続台帳の合成回帰。実 Session の回帰とは分けて検査する。
use super::super::testkit;
use super::*;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = crate::test_util::unique_temp_dir("zaivern-team-test", "restore-writer-proof");
        std::fs::create_dir_all(root.join(".zai-team-worktrees/restore/part-1")).unwrap();
        Self(root)
    }

    fn store(&self) -> PathBuf {
        self.0.join(".zai-team-worktrees/restore/part-1.lease.json")
    }

    fn runtime(&self, state: TeamTaskState, successor: TeamTaskState) -> TeamRuntime {
        let mut task = testkit::task(1, "implementation", &[]);
        task.files = vec![".zai-team-worktrees/restore/part-1/**".into()];
        task.dispatch_seq = 1;
        task.state = state;
        task.last_summary = "受理済みの成果報告".into();
        task.validation_commands.clear();
        let mut next = testkit::task(2, "successor", &[1]);
        next.state = successor;
        next.validation_commands.clear();
        let mut rt = TeamRuntime::from_plan(
            TeamPlan {
                goal: testkit::goal(),
                teams: vec![],
                tasks: vec![task, next],
            },
            self.0.clone(),
            RunOptions {
                review_required: false,
                ..RunOptions::default()
            },
        );
        rt.goal.status = GoalStatus::Running;
        TeamRuntime::restore_in(rt.to_saved(), self.0.clone(), self.0.clone())
    }

    fn released(&self) {
        std::fs::write(self.store(), br#"{"leases":[]}"#).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// 背景照合の応答を受けてから実際の poll 経路へ戻す。時間の経過を
// 「照合が終わった」「writer が終了した」という証拠にはしない。
fn finish_probe(rt: &mut TeamRuntime) {
    rt.writer_probe_at = Instant::now();
    rt.poll_restored_writers();
    let result = rt
        .writer_probe
        .take()
        .expect("背景照合を開始する")
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("照合の応答");
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(result).unwrap();
    rt.writer_probe = Some(rx);
    rt.poll_restored_writers();
}

#[test]
fn missing_empty_and_corrupt_proof_hold_validation_until_valid_release() {
    for content in [None, Some(""), Some("   "), Some("{}"), Some("{broken")] {
        let f = Fixture::new();
        if let Some(content) = content {
            std::fs::write(f.store(), content).unwrap();
        }
        let mut rt = f.runtime(TeamTaskState::Validating, TeamTaskState::Pending);
        finish_probe(&mut rt);
        assert!(rt.task_writer_pending(1), "不明な証拠: {content:?}");
        rt.advance(&mut vec![]);
        rt.promote_ready();
        assert_eq!(rt.task(1).unwrap().state, TeamTaskState::Validating);
        assert_eq!(rt.task(2).unwrap().state, TeamTaskState::Pending);
        assert_eq!(rt.task(1).unwrap().last_summary, "受理済みの成果報告");

        let mut restored = TeamRuntime::restore_in(rt.to_saved(), f.0.clone(), f.0.clone());
        finish_probe(&mut restored);
        assert!(restored.task_writer_pending(1), "再保存で待機を迂回しない");
        f.released();
        finish_probe(&mut restored);
        assert!(!restored.task_writer_pending(1));
        restored.advance(&mut vec![]);
        restored.promote_ready();
        assert_eq!(restored.task(1).unwrap().state, TeamTaskState::Completed);
        assert_eq!(restored.task(2).unwrap().state, TeamTaskState::Ready);
    }
}

#[test]
fn submitted_and_completed_dependencies_wait_even_when_successor_was_saved_ready() {
    for state in [TeamTaskState::Submitted, TeamTaskState::Completed] {
        for successor in [TeamTaskState::Pending, TeamTaskState::Ready] {
            let f = Fixture::new();
            let mut rt = f.runtime(state, successor);
            finish_probe(&mut rt);
            rt.promote_ready();
            assert_eq!(rt.task(2).unwrap().state, successor);
            let agent = rt.agents[0].id.clone();
            rt.bind_session(&agent, 91, None);
            rt.agents[0].state = AgentWorkState::Idle;
            let mut effects = vec![];
            rt.dispatch(&mut effects);
            assert!(!effects
                .iter()
                .any(|e| matches!(e, TeamEffect::SendInstruction { task: 2, .. })));
            assert_ne!(rt.task(2).unwrap().state, TeamTaskState::Running);
            f.released();
            finish_probe(&mut rt);
            rt.promote_ready();
            effects.clear();
            rt.dispatch(&mut effects);
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, TeamEffect::SendInstruction { task: 2, .. })),
                "終了証拠後に配送する: {state:?}, {successor:?}"
            );
        }
    }
}

#[test]
fn already_released_proof_does_not_strand_restored_terminal_goal() {
    for state in [TeamTaskState::Submitted, TeamTaskState::Completed] {
        let f = Fixture::new();
        f.released();
        let mut rt = f.runtime(state, TeamTaskState::Completed);
        rt.goal.status = if state == TeamTaskState::Submitted {
            GoalStatus::Submitted
        } else {
            GoalStatus::Completed
        };
        let mut rt = TeamRuntime::restore_in(rt.to_saved(), f.0.clone(), f.0.clone());
        assert!(
            !rt.goal.status.is_terminal(),
            "終了証拠の照合前は全体完了を確定しない"
        );
        finish_probe(&mut rt);
        assert!(!rt.task_writer_pending(1));
        rt.update_goal();
        assert_eq!(
            rt.goal.status,
            if state == TeamTaskState::Submitted {
                GoalStatus::Submitted
            } else {
                GoalStatus::Completed
            }
        );
    }
}

#[test]
fn unreadable_ledger_holds_validation_and_recovers_after_readability_returns() {
    let f = Fixture::new();
    // A directory cannot be read as a ledger even when tests run as root.
    std::fs::create_dir(f.store()).unwrap();
    let mut rt = f.runtime(TeamTaskState::Validating, TeamTaskState::Pending);
    finish_probe(&mut rt);
    assert!(rt.task_writer_pending(1));
    rt.advance(&mut vec![]);
    assert_eq!(rt.task(1).unwrap().state, TeamTaskState::Validating);
    std::fs::remove_dir(f.store()).unwrap();
    f.released();
    finish_probe(&mut rt);
    rt.advance(&mut vec![]);
    assert_eq!(rt.task(1).unwrap().state, TeamTaskState::Completed);
}

#[test]
fn a_restored_writer_wait_does_not_reserve_the_only_agent_from_independent_work() {
    let f = Fixture::new();
    let mut rt = f.runtime(TeamTaskState::Submitted, TeamTaskState::Ready);
    finish_probe(&mut rt);
    let mut independent = testkit::task(3, "independent", &[]);
    independent.state = TeamTaskState::Ready;
    rt.tasks.push(independent);
    let agent = rt.agents[0].id.clone();
    rt.bind_session(&agent, 91, None);
    rt.agents[0].state = AgentWorkState::Idle;
    let mut effects = vec![];
    rt.dispatch(&mut effects);
    assert!(!effects
        .iter()
        .any(|e| matches!(e, TeamEffect::SendInstruction { task: 2, .. })));
    assert!(effects
        .iter()
        .any(|e| matches!(e, TeamEffect::SendInstruction { task: 3, .. })));
}

#[test]
fn a_finished_direct_dependency_does_not_hide_a_pending_ancestor_writer() {
    let f = Fixture::new();
    let mut rt = f.runtime(TeamTaskState::Completed, TeamTaskState::Completed);
    rt.tasks[1].files = vec![".zai-team-worktrees/restore/part-2/**".into()];
    rt.tasks[1].dispatch_seq = 1;
    std::fs::write(
        f.0.join(".zai-team-worktrees/restore/part-2.lease.json"),
        br#"{"leases":[]}"#,
    )
    .unwrap();
    let mut next = testkit::task(3, "indirect-successor", &[2]);
    next.state = TeamTaskState::Ready;
    rt.tasks.push(next);
    let mut rt = TeamRuntime::restore_in(rt.to_saved(), f.0.clone(), f.0.clone());
    finish_probe(&mut rt);
    assert!(rt.task_writer_pending(1));
    assert!(!rt.task_writer_pending(2), "直接依存のwriterは既に終了済み");
    let agent = rt.agents[0].id.clone();
    rt.bind_session(&agent, 91, None);
    rt.agents[0].state = AgentWorkState::Idle;
    let mut effects = vec![];
    rt.dispatch(&mut effects);
    assert!(!effects
        .iter()
        .any(|e| matches!(e, TeamEffect::SendInstruction { task: 3, .. })));
    f.released();
    finish_probe(&mut rt);
    rt.dispatch(&mut effects);
    assert!(effects
        .iter()
        .any(|e| matches!(e, TeamEffect::SendInstruction { task: 3, .. })));
}
