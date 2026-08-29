//! テスト用の下ごしらえ。**製品コードからは呼ばない** (`#[cfg(test)]`)。
//!
//! 各モジュールのテストが同じ形のタスクを作り直すと、1 か所直すたびに
//! 全部を直すことになる。ここに 1 本だけ置く。

#![cfg(test)]

use super::model::{GoalId, TeamGoal, TeamId, TeamRole, TeamTask, TeamTaskState, ValidationState};

/// 検証可能な最小のタスク。
pub fn task(id: u64, key: &str, deps: &[u64]) -> TeamTask {
    TeamTask {
        id,
        goal_id: GoalId::new("g1"),
        key: key.to_string(),
        title: key.to_string(),
        description: String::new(),
        team_id: TeamId::new("implementation"),
        role: TeamRole::Implementer,
        dependencies: deps.to_vec(),
        files: Vec::new(),
        required_caps: Vec::new(),
        acceptance_criteria: vec!["動作する".to_string()],
        validation_commands: vec!["cargo test".to_string()],
        state: TeamTaskState::Pending,
        assigned_agent: None,
        assigned_session: None,
        attempts: 0,
        review_of: None,
        coordinator_task: None,
        validation: ValidationState::default(),
        review: Default::default(),
        context: Vec::new(),
        last_summary: String::new(),
        changed_files: Vec::new(),
        blockers: Vec::new(),
        created_at: 1,
        updated_at: 1,
    }
}

/// 最小の Goal。
pub fn goal() -> TeamGoal {
    TeamGoal::new(
        GoalId::new("g1"),
        "テスト用 Goal",
        "SPEC 本文",
        vec!["実装が終わっている".to_string()],
    )
}
