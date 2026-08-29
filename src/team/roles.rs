//! エージェントの作業状態を**既存の真実から導出する**層。
//!
//! ## UI 専用の第 2 の真実を作らない
//!
//! [`super::model::AgentWorkState`] は保存もするが、**そこが真実ではない**。
//! 真実は
//!
//!   * [`crate::coordinator::SessionState`] (プロセスと画面から出る)
//!   * タスクの [`super::model::TeamTaskState`]
//!   * 検証 ([`ValidationState`]) とレビュー ([`ReviewState`])
//!
//! の 3 つで、ここはそれを 1 つの表示用の値へ**畳むだけ**。畳み方が
//! 1 か所にあるから、「Cockpit では Working なのに Organization Board では
//! Idle」という食い違いが構造的に起こらない。
//!
//! ## 優先順位 (テストで固定する)
//!
//! 1. `Exited` — プロセスが居ないなら、他の何より先に出す
//! 2. `WaitingApproval` — 人が押すまで 1 バイトも進まない
//! 3. `Stalled` — 進んでいないことは、何をしている「はず」かより重要
//! 4. `Blocked` — タスクが詰まっている
//! 5. `Reviewing` / `Testing` — いま何をしているか
//! 6. `Working` / `Planning` / `Coordinating`
//! 7. `Completed` / `Idle` / `Unknown`

use crate::coordinator::SessionState;

use super::model::{
    AgentWorkState, ReviewState, TeamRole, TeamTask, TeamTaskState, ValidationState,
};

/// セッション・タスク・検証・レビューから作業状態を決める (純関数)。
pub fn derive_agent_work_state(
    session: SessionState,
    task: Option<&TeamTask>,
    validation: Option<&ValidationState>,
    review: Option<&ReviewState>,
) -> AgentWorkState {
    // 1) プロセスが居ない。ここに曖昧さは無い。
    if session == SessionState::Exited {
        return AgentWorkState::Exited;
    }
    // 2) 承認待ち。**Working より必ず優先**する — 実際には 1 バイトも
    //    進んでいないのに「作業中」と出すと、人が気付かず放置される。
    if session == SessionState::WaitingApproval {
        return AgentWorkState::WaitingApproval;
    }
    // 3) 停滞。**Testing より優先**する。検証を始めた記録が残っていても、
    //    出力が動いていないなら止まっている。
    if session == SessionState::Stalled {
        return AgentWorkState::Stalled;
    }
    // 4) タスクが詰まっている。
    if task.is_some_and(|t| t.state == TeamTaskState::Blocked) {
        return AgentWorkState::Blocked;
    }
    if task.is_some_and(|t| t.state == TeamTaskState::NeedsUser) {
        return AgentWorkState::Blocked;
    }
    // 5) いま何をしているか。レビュー → 検証の順で見る
    //    (レビュー中のセッションが検証も回していることはあるため)。
    if review.is_some_and(|r| r.running) {
        return AgentWorkState::Reviewing;
    }
    if validation.is_some_and(|v| v.running) {
        return AgentWorkState::Testing;
    }
    if let Some(t) = task {
        match t.state {
            TeamTaskState::Reviewing => return AgentWorkState::Reviewing,
            TeamTaskState::Validating => return AgentWorkState::Testing,
            TeamTaskState::Completed => return AgentWorkState::Completed,
            TeamTaskState::Running | TeamTaskState::Assigned | TeamTaskState::RevisionRequired => {
                return match t.role {
                    TeamRole::Planner => AgentWorkState::Planning,
                    TeamRole::Architect => AgentWorkState::Planning,
                    TeamRole::TeamLead => AgentWorkState::Coordinating,
                    TeamRole::Tester => AgentWorkState::Testing,
                    TeamRole::Reviewer => AgentWorkState::Reviewing,
                    _ => AgentWorkState::Working,
                }
            }
            _ => {}
        }
    }
    // 6) タスクを持っていない。
    match session {
        SessionState::Idle => AgentWorkState::Idle,
        SessionState::Working => AgentWorkState::Working,
        _ => AgentWorkState::Unknown,
    }
}

/// この役割は実装 (コードを書く) をするか。
///
/// レビュアーに書かせないための判定で、指示文 ([`super::prompt`]) と
/// 割り当て ([`super::scheduler`]) の両方が同じ答えを見る。
pub fn writes_code(role: TeamRole) -> bool {
    matches!(role, TeamRole::Implementer | TeamRole::Integrator)
}

/// この役割はレビューか。
pub fn is_review_role(role: TeamRole) -> bool {
    matches!(role, TeamRole::Reviewer | TeamRole::Tester)
}

#[cfg(test)]
mod tests {
    use super::super::testkit::task;
    use super::*;

    fn t(state: TeamTaskState, role: TeamRole) -> TeamTask {
        let mut x = task(1, "a", &[]);
        x.state = state;
        x.role = role;
        x
    }

    #[test]
    fn 承認待ちは作業中より優先() {
        let task = t(TeamTaskState::Running, TeamRole::Implementer);
        assert_eq!(
            derive_agent_work_state(SessionState::WaitingApproval, Some(&task), None, None),
            AgentWorkState::WaitingApproval
        );
    }

    #[test]
    fn 停滞は検証中より優先() {
        let task = t(TeamTaskState::Validating, TeamRole::Implementer);
        let v = ValidationState {
            running: true,
            runs: Vec::new(),
            generation: 0,
        };
        assert_eq!(
            derive_agent_work_state(SessionState::Stalled, Some(&task), Some(&v), None),
            AgentWorkState::Stalled
        );
    }

    #[test]
    fn 詰まりは作業中より優先() {
        let task = t(TeamTaskState::Blocked, TeamRole::Implementer);
        assert_eq!(
            derive_agent_work_state(SessionState::Working, Some(&task), None, None),
            AgentWorkState::Blocked
        );
    }

    #[test]
    fn 終了は端末の状態が最優先() {
        let task = t(TeamTaskState::Running, TeamRole::Implementer);
        let v = ValidationState {
            running: true,
            runs: Vec::new(),
            generation: 0,
        };
        let r = ReviewState {
            running: true,
            ..Default::default()
        };
        assert_eq!(
            derive_agent_work_state(SessionState::Exited, Some(&task), Some(&v), Some(&r)),
            AgentWorkState::Exited
        );
    }

    #[test]
    fn レビュー中はreviewing() {
        let task = t(TeamTaskState::Reviewing, TeamRole::Reviewer);
        assert_eq!(
            derive_agent_work_state(SessionState::Working, Some(&task), None, None),
            AgentWorkState::Reviewing
        );
    }

    #[test]
    fn 検証実行中はtesting() {
        let task = t(TeamTaskState::Validating, TeamRole::Implementer);
        let v = ValidationState {
            running: true,
            runs: Vec::new(),
            generation: 0,
        };
        assert_eq!(
            derive_agent_work_state(SessionState::Working, Some(&task), Some(&v), None),
            AgentWorkState::Testing
        );
    }

    #[test]
    fn タスクが無ければセッションの状態そのまま() {
        assert_eq!(
            derive_agent_work_state(SessionState::Idle, None, None, None),
            AgentWorkState::Idle
        );
        assert_eq!(
            derive_agent_work_state(SessionState::Working, None, None, None),
            AgentWorkState::Working
        );
    }

    #[test]
    fn 役割ごとに実装中の見え方が変わる() {
        for (role, want) in [
            (TeamRole::Implementer, AgentWorkState::Working),
            (TeamRole::Planner, AgentWorkState::Planning),
            (TeamRole::Architect, AgentWorkState::Planning),
            (TeamRole::TeamLead, AgentWorkState::Coordinating),
            (TeamRole::Tester, AgentWorkState::Testing),
            (TeamRole::Reviewer, AgentWorkState::Reviewing),
        ] {
            let task = t(TeamTaskState::Running, role);
            assert_eq!(
                derive_agent_work_state(SessionState::Working, Some(&task), None, None),
                want,
                "{}",
                role.key()
            );
        }
    }

    #[test]
    fn 役割の分類() {
        assert!(writes_code(TeamRole::Implementer));
        assert!(writes_code(TeamRole::Integrator));
        assert!(!writes_code(TeamRole::Reviewer));
        assert!(is_review_role(TeamRole::Reviewer));
        assert!(!is_review_role(TeamRole::Implementer));
    }
}
