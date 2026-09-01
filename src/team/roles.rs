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
    // **停滞と呼ぶのは、仕事を持っているときだけ。**
    //
    // 持ち仕事が無い担当は出力が動かなくて当たり前なので、そのまま
    // 「停滞」と読まれる。そして停滞には配らないので**二度と出力が
    // 動かない** — 推測が自分で自分を裏付ける輪になる。実測で、空いている
    // 担当が 3 体居るのにタスクが 5 本待ち続けた。
    if session == SessionState::Stalled && task.is_some() {
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
    //
    // **`Stalled` はここでは `Idle`。** 仕事が無いのだから出力が無いのは
    // 当然で、配れる状態である。`Unknown` (画面が読めない) は据え置く —
    // 読めない相手へ配ってよいかは調停層 (`coordinator::assignable`) が決める。
    match session {
        SessionState::Idle | SessionState::Stalled => AgentWorkState::Idle,
        SessionState::AwaitingInput => AgentWorkState::Idle,
        SessionState::Working => AgentWorkState::Working,
        _ => AgentWorkState::Unknown,
    }
}

/// エージェントプリセット 1 つぶんの手掛かり。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresetRow {
    /// 設定に書かれた名前 (`Claude Code` など)。
    pub name: String,
    /// AI CLI として使えるか (素のシェルは対象外)。
    pub is_ai: bool,
    /// **この PC で実際に起動できるか** (実体が PATH にある)。
    ///
    /// 入っていない CLI を割り当てると、その担当だけが永久に起動しない。
    /// 「使える 1 本を全員で使う」ほうが、動かない担当を作るよりよい。
    pub available: bool,
}

/// **役割に合うエージェントプリセットを選ぶ** (純関数)。
///
/// 判断は 3 段:
///
/// 1. 名前に役割の綴り (`reviewer` / `tester` …) を含む、起動できる AI CLI
/// 2. **起動できる AI CLI を役割ごとに配る** — 同じものを全員に割り当てず、
///    入っている CLI の数だけ担当を散らす
/// 3. どれも起動できないなら、AI CLI のうち最初のもの (従来どおり)
///
/// ## なぜ散らすか
///
/// 全員が同じ CLI だと、その CLI の癖 (見落とし・書き癖) がチーム全体に
/// 同じ形で乗る。レビューを別の CLI にできるなら、**実装が見落としたものを
/// 別の目が見る**。入っているものを使わない理由が無い。
///
/// ## 決め方は固定
///
/// 役割の並び順 ([`TeamRole::ALL`]) の中での位置を、使える CLI の本数で
/// 割った余りにする。**同じ顔ぶれなら毎回同じ割り当て**になるので、
/// 「今日は誰がどれ」で結果が変わらない。
pub fn preset_for_role(presets: &[PresetRow], role: TeamRole) -> Option<usize> {
    let want = role.key();
    // 1) 役割の名前を持つプリセット (人が明示的に用意したもの)。
    if let Some(i) = presets
        .iter()
        .position(|p| p.is_ai && p.available && p.name.to_ascii_lowercase().contains(want))
    {
        return Some(i);
    }
    // 2) 起動できるものを役割ごとに配る。
    let usable: Vec<usize> = presets
        .iter()
        .enumerate()
        .filter(|(_, p)| p.is_ai && p.available)
        .map(|(i, _)| i)
        .collect();
    if !usable.is_empty() {
        let slot = TeamRole::ALL.iter().position(|r| *r == role).unwrap_or(0);
        return Some(usable[slot % usable.len()]);
    }
    // 3) 起動できるものが 1 つも分からない (PATH を引けない等) —
    //    従来どおり最初の AI CLI へ落とす。**担当を 0 体にしない。**
    presets.iter().position(|p| p.is_ai)
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

    pub(super) fn t(state: TeamTaskState, role: TeamRole) -> TeamTask {
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

    fn row(name: &str, is_ai: bool, available: bool) -> PresetRow {
        PresetRow {
            name: name.to_string(),
            is_ai,
            available,
        }
    }

    #[test]
    fn 役割に合うプリセットを選ぶ() {
        use TeamRole::*;
        let presets = vec![
            row("Shell", false, true),
            row("Claude", true, true),
            row("Reviewer bot", true, true),
        ];
        // 名前に役割の綴りがあれば、それ
        assert_eq!(preset_for_role(&presets, Reviewer), Some(2));
        // AI CLI が 1 つも無ければ選べない (**素のシェルは選ばない**)
        let none = vec![row("Shell", false, true)];
        assert_eq!(preset_for_role(&none, Implementer), None);
    }

    /// **入っている CLI を役割ごとに配る。**
    ///
    /// 全員が同じ CLI だと、その CLI の癖がチーム全体に同じ形で乗る
    /// (実装の見落としを、同じ見落とし方をする相手がレビューする)。
    #[test]
    fn 入っているclitを役割ごとに散らす() {
        use TeamRole::*;
        let two = vec![row("Claude", true, true), row("Codex", true, true)];
        // 役割の並び順で交互に配る。**同じ顔ぶれなら毎回同じ割り当て。**
        let got: Vec<Option<usize>> = TeamRole::ALL
            .iter()
            .map(|r| preset_for_role(&two, *r))
            .collect();
        assert_eq!(
            got,
            vec![
                Some(0),
                Some(1),
                Some(0),
                Some(1),
                Some(0),
                Some(1),
                Some(0)
            ]
        );
        // 2 回呼んでも同じ (決め方が固定されている)。
        assert_eq!(preset_for_role(&two, Reviewer), preset_for_role(&two, Reviewer));
        // 使えるものが 1 本しか無ければ、全員それになる。
        let one = vec![row("Claude", true, true), row("Codex", true, false)];
        for r in TeamRole::ALL {
            assert_eq!(preset_for_role(&one, r), Some(0), "{r:?}");
        }
    }

    /// **入っていないものを割り当てない。** 割り当てると、その担当だけが
    /// 永久に起動しない (画面には居るのに何も起きない)。
    #[test]
    fn 起動できないcliには配らない() {
        use TeamRole::*;
        // 名前が役割と一致していても、入っていなければ選ばない。
        let p = vec![
            row("Claude", true, true),
            row("Reviewer bot", true, false),
        ];
        assert_eq!(preset_for_role(&p, Reviewer), Some(0));
        // どれも起動を確かめられないときは、担当を 0 体にせず最初の AI CLI へ。
        let unknown = vec![row("Shell", false, false), row("Claude", true, false)];
        assert_eq!(preset_for_role(&unknown, Implementer), Some(1));
    }
}

#[cfg(test)]
mod no_task_no_stall_tests {
    use super::tests::t;
    use super::*;

    /// **仕事を持っていない担当は停滞ではない。**
    ///
    /// 実測の止まり方: Planner / Tester / Reviewer が `stalled` で、`ready` の
    /// タスクが 5 本待っていた。持ち仕事が無ければ出力が動かなくて当たり前
    /// なのに、それを停滞と読み、停滞には配らないので**二度と出力が動かない**。
    /// 推測が自分で自分を裏付ける輪になっていた。
    ///
    /// ここが直し場である。スケジューラ側 (`Candidate::free`) を緩めて
    /// 直そうとすると、調停層が別の規則で断り、**提案しては断られる**組み合わせで
    /// 台帳が埋まる (実測 500 件)。
    #[test]
    fn 仕事が無い担当は停滞ではない() {
        // 仕事を持っていない → 配れる状態として出す。
        assert_eq!(
            derive_agent_work_state(SessionState::Stalled, None, None, None),
            AgentWorkState::Idle,
            "手ぶらの担当を停滞と呼んでいる (二度と配られなくなる)"
        );
        assert_eq!(
            derive_agent_work_state(SessionState::AwaitingInput, None, None, None),
            AgentWorkState::Idle
        );
        // **仕事を持っているのに動かないのは、本当の停滞。**
        let t = t(TeamTaskState::Running, TeamRole::Implementer);
        assert_eq!(
            derive_agent_work_state(SessionState::Stalled, Some(&t), None, None),
            AgentWorkState::Stalled,
            "担当を持ったまま止まっている相手を見逃している"
        );
        // 読めない相手は据え置く (配ってよいかは調停層が決める)。
        assert_eq!(
            derive_agent_work_state(SessionState::Unknown, None, None, None),
            AgentWorkState::Unknown
        );
    }
}
