//! **落ちても消えず、二度も走らない** — Effect の crash consistency。
//!
//! ここで確かめるのは正常系ではなく、**外部副作用が成立する直前と直後に
//! 落ちた**ときの振る舞い。Effect の台帳 (`RunDoc::effects`) は
//!
//! ```text
//! 発行 (Dispatched) → 実行側が本当に成立させた (Completed)
//!                   → 成立しなかった (記録ごと外す = 再発行できる)
//! ```
//!
//! という 3 通りしか持たない。**「渡した」と「成立した」を同じ段に
//! しない**のがこの層の全部で、混ぜた瞬間に
//!
//! * 積めたのに届かなかった指示が「送った」ことになって永久に消える
//! * 起こしたのに保存前に落ちたエージェントが、次の起動で 2 体になる
//!
//! のどちらかが起きる。
//!
//! ## 落ちるところをどう作るか
//!
//! `to_saved()` で**そのときの記録を撮り**、Runtime を捨てて
//! `restore()` で立て直す。「その瞬間に落ちて、次に起動した」と同じ状態に
//! なる (プロセスを落とさずに決定的に作れる)。

use std::path::PathBuf;

use super::model::*;
use super::runtime::*;
use super::runtime_tests::{obs_for_test, started_for_test};

fn ws() -> PathBuf {
    PathBuf::from("/zaivern-team-crash-workspace")
}

/// 落ちて、立て直す。**記録に残っていたものだけが残る。**
fn crash_and_restart(rt: &TeamRuntime) -> TeamRuntime {
    TeamRuntime::restore(rt.to_saved(), ws())
}

/// 起動要求のセッションを結び付ける (偽の起動)。目印も一緒に覚える。
fn launch_all(rt: &mut TeamRuntime, effects: &[TeamEffect], next: &mut SessionId) -> Vec<SessionId> {
    let mut out = Vec::new();
    for e in effects {
        if let TeamEffect::StartAgent(s) = e {
            let identity = format!("/logs/{}.log", s.agent_id);
            rt.bind_session(&s.agent_id, *next, Some(identity));
            rt.note_effect_done(&e.key());
            out.push(*next);
            *next += 1;
        }
    }
    out
}

/// 発行された Effect の冪等キー。
fn keys(effects: &[TeamEffect]) -> Vec<String> {
    effects.iter().map(|e| e.key()).collect()
}

/// 指示の Effect だけ取り出す。
fn instructions(effects: &[TeamEffect]) -> Vec<(TaskId, String)> {
    effects
        .iter()
        .filter_map(|e| match e {
            TeamEffect::SendInstruction { task, key, .. } => Some((*task, key.clone())),
            _ => None,
        })
        .collect()
}

/// 出てきた「人の承認待ち」を全部承認する。
///
/// `cargo test` はリポジトリ内のコードを走らせるので、承認を通らない限り
/// 検証は 1 行も走らない (それがこの製品の約束)。crash の検査で見たいのは
/// **承認したあとの成立の仕方**なので、ここは通す。
fn approve_all(rt: &mut TeamRuntime, effects: &[TeamEffect]) {
    let ids: Vec<EventId> = effects
        .iter()
        .filter_map(|e| match e {
            TeamEffect::RequestHumanApproval(d) => Some(d.id),
            _ => None,
        })
        .collect();
    for id in ids {
        rt.apply_action(TeamAction::ApproveDecision(id));
    }
}

/// 起動要求だけ取り出す。
fn starts(effects: &[TeamEffect]) -> Vec<AgentLaunchSpec> {
    effects
        .iter()
        .filter_map(|e| match e {
            TeamEffect::StartAgent(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// 検証要求だけ取り出す。
fn validations(effects: &[TeamEffect]) -> Vec<ValidationSpec> {
    effects
        .iter()
        .filter_map(|e| match e {
            TeamEffect::RunValidation(v) => Some(v.clone()),
            _ => None,
        })
        .collect()
}

/// 起動 → 結び付け、まで進めた Runtime とそのセッション。
///
/// **割り当ての刻みは呼ぶ側が回す。** 割り当てと指示は同じ刻みで出るので、
/// ここで回してしまうと、指示を 1 通も見ないまま先へ進む。
fn launched(agents: usize) -> (TeamRuntime, Vec<SessionId>) {
    let mut rt = started_for_test(agents);
    let mut next: SessionId = 1;
    let first = rt.tick(&obs_for_test(1, &[]));
    let sessions = launch_all(&mut rt, &first, &mut next);
    (rt, sessions)
}

// ── A: 指示を積んだ直後に落ちる ──────────────────────────────────────

#[test]
fn 指示は積めただけでは完了にせず落ちても消えない() {
    let (mut rt, sessions) = launched(2);
    let eff = rt.tick(&obs_for_test(2, &sessions));
    let sent = instructions(&eff);
    assert!(!sent.is_empty(), "指示が 1 通も出ない");
    let (task, key) = sent[0].clone();

    // **積めた。まだ届いていない。** ここで完了にしないのが今回の修正。
    // 同じ刻みをもう一度回しても、二重には出ない (発行済みの記録が効く)。
    let again = rt.tick(&obs_for_test(4, &sessions));
    assert!(
        !keys(&again).contains(&key),
        "積んだだけの指示をもう一度出した (二重送信): {:?}",
        keys(&again)
    );

    // ★ ここで落ちる。**成立していない Effect は記録に残らない**ので、
    //   立て直した側は「まだ何もしていない」から始める。
    let mut rt2 = crash_and_restart(&rt);
    assert!(
        !rt2.effect_completed(&key),
        "届いていない指示が完了として引き継がれた"
    );
    // 立て直したセッションを結び直せば、担当は配り直され、指示は出る。
    let first = rt2.tick(&obs_for_test(10, &[]));
    let mut next: SessionId = 100;
    let s2 = launch_all(&mut rt2, &first, &mut next);
    // 割り当てと指示は同じ刻みで出る。
    let eff2 = rt2.tick(&obs_for_test(11, &s2));
    let sent2 = instructions(&eff2);
    assert!(
        sent2.iter().any(|(t, _)| *t == task),
        "落ちたあと #{task} への指示が消えた: {:?}",
        instructions(&eff2)
    );
    // **同じ鍵は二度と使わない** (配り直しは新しい指示)。
    assert!(
        !sent2.iter().any(|(_, k)| *k == key),
        "前の指示の鍵をそのまま使い回した"
    );
}

#[test]
fn 届いた指示は二度と出ないが届かなかった指示は配り直す() {
    let (mut rt, sessions) = launched(2);
    let eff = rt.tick(&obs_for_test(2, &sessions));
    let sent = instructions(&eff);
    assert!(sent.len() >= 2, "2 体ぶんの指示が出ていない: {sent:?}");
    let (ok_task, ok_key) = sent[0].clone();
    let (ng_task, ng_key) = sent[1].clone();

    // 片方は届いた。もう片方は宛先が消えて届かなかった。
    rt.note_effect_done(&ok_key);
    rt.note_instruction_undelivered(ng_task, &ng_key, "宛先の端末が応答しませんでした");

    // 届いたほうは完了のまま。落ちても完了のまま = 二度と出ない。
    assert!(rt.effect_completed(&ok_key));
    // 届かなかったほうは担当が外れ、配り直せる状態に戻っている。
    let t = rt.task(ng_task).expect("タスク");
    assert!(
        t.assigned_session.is_none() && t.assigned_agent.is_none(),
        "届かなかったのに担当を握ったまま: {t:?}"
    );
    assert!(
        matches!(t.state, TeamTaskState::Ready | TeamTaskState::NeedsUser),
        "配り直せない状態のまま止まった: {:?}",
        t.state
    );
    assert!(!rt.effect_completed(&ng_key), "届いていないのに完了にした");
    // 理由が残る (人が読める形で)。
    assert!(
        t.context.iter().any(|c| c.contains("届きません")),
        "届かなかった理由が残っていない: {:?}",
        t.context
    );
    // 配り直しでは**新しい鍵**が出る (前の鍵は抑止されたままでよい)。
    let eff2 = rt.tick(&obs_for_test(5, &sessions));
    let again = instructions(&eff2);
    assert!(
        again.iter().any(|(t, k)| *t == ng_task && *k != ng_key),
        "配り直しの指示が出ない: {again:?}"
    );
    assert!(
        !again.iter().any(|(t, _)| *t == ok_task),
        "届いた指示をもう一度出した"
    );
}

#[test]
fn 届かないままでも無限には配り直さない() {
    // 何度でも配り直すと、同じ相手へ延々と積み直す無限ループになる。
    // 既存の試行上限で止まり、人へ上がること。
    let (mut rt, sessions) = launched(2);
    let eff = rt.tick(&obs_for_test(2, &sessions));
    let (task, _) = instructions(&eff)[0].clone();
    let max = rt.run().max_attempts;
    let mut now = 4;
    for _ in 0..(max as u64 + 2) {
        // **いま待っている指示の結末**として返す (古い鍵は無視されるので、
        // 毎回いまの鍵を引き直す — 実物も同じ経路を通る)。
        if let Some(k) = rt.current_instruction_key(task) {
            rt.note_instruction_undelivered(task, &k, "応答しません");
        }
        rt.tick(&obs_for_test(now, &sessions));
        now += 1;
    }
    let t = rt.task(task).expect("タスク");
    assert_eq!(
        t.state,
        TeamTaskState::NeedsUser,
        "上限を超えても配り直し続けている: {t:?}"
    );
    assert!(t.attempts >= max, "試行として数えていない: {}", t.attempts);
}

#[test]
fn 古い配達の結末は新しい担当を剥がさない() {
    // 配達の結末は**遅れて届く**。その間にタスクが先へ進んでいることが
    // ある (相手は本当は受け取っていて、もう次の段に居る)。古い結末で
    // 担当を剥がすと、出来上がっている成果を捨てて作り直させることになる。
    let (mut rt, sessions) = launched(2);
    let eff = rt.tick(&obs_for_test(2, &sessions));
    let (task, old_key) = instructions(&eff)[0].clone();
    let before = rt.task(task).expect("タスク").clone();

    // 一度届かなかったことにして配り直す → 鍵が変わる。
    rt.note_instruction_undelivered(task, &old_key, "応答しません");
    let eff2 = rt.tick(&obs_for_test(3, &sessions));
    let new_key = instructions(&eff2)
        .into_iter()
        .find(|(t, _)| *t == task)
        .map(|(_, k)| k)
        .expect("配り直しの指示");
    assert_ne!(new_key, old_key, "配り直しで鍵が変わっていない");
    let now = rt.task(task).expect("タスク").clone();

    // ★ ここへ**古い配達**の結末が遅れて届く。
    rt.note_instruction_undelivered(task, &old_key, "応答しません");
    let after = rt.task(task).expect("タスク");
    assert_eq!(
        after.assigned_agent, now.assigned_agent,
        "古い配達の結末で新しい担当を剥がした"
    );
    assert_eq!(after.state, now.state, "古い配達の結末で状態を巻き戻した");
    assert_eq!(
        after.attempts, now.attempts,
        "古い配達の結末を試行として二重に数えた"
    );
    let _ = before;
}

// ── B: 起動した直後に落ちる ──────────────────────────────────────────

#[test]
fn 起動して保存したあとに落ちても同じエージェントを二体起こさない() {
    let mut rt = started_for_test(2);
    let mut next: SessionId = 1;
    let first = rt.tick(&obs_for_test(1, &[]));
    let sessions = launch_all(&mut rt, &first, &mut next);
    assert!(!sessions.is_empty(), "起動要求が出ない");
    let launched: Vec<AgentId> = starts(&first).iter().map(|s| s.agent_id.clone()).collect();

    // ★ ここで落ちる (結び付けと目印は保存済み)。
    let mut rt2 = crash_and_restart(&rt);
    // セッション ID は再起動で意味を失うので必ず外れている。
    assert!(
        rt2.agents().iter().all(|a| a.session_id.is_none()),
        "前のプロセスのセッション ID を引き継いだ"
    );
    // **目印は残る。** これが「もう起こしてある」の唯一の手がかり。
    for id in &launched {
        let a = rt2.agent(id).expect("エージェント");
        assert_eq!(
            a.session_identity.as_deref(),
            Some(format!("/logs/{id}.log").as_str()),
            "目印を落とした ({id})"
        );
    }
    // 起動要求は**引き取り先を載せて**出る。実行側はこれを見て、
    // 生きているセッションがあれば起こさずに結び直す。
    let eff = rt2.tick(&obs_for_test(10, &[]));
    let again = starts(&eff);
    assert!(!again.is_empty(), "立て直したのに起動要求が出ない");
    for s in &again {
        assert_eq!(
            s.adopt,
            Some(format!("/logs/{}.log", s.agent_id)),
            "引き取り先を載せていない (実行側は 2 体目を起こすしかない)"
        );
    }
}

#[test]
fn 引き取り先の判断は目印と名前だけで決まる() {
    use super::launch::{adopt_choice, SessionFact};
    let ws = PathBuf::from("/w");
    let fact = |id: SessionId, identity: &str, title: &str, running: bool, bound: bool| {
        SessionFact {
            id,
            identity: identity.into(),
            title: title.into(),
            cwd: ws.clone(),
            running,
            bound,
        }
    };
    let all = vec![
        fact(1, "/logs/other.log", "別の人", true, false),
        fact(2, "/logs/a.log", "Implementer #1", true, false),
        fact(3, "", "Implementer #1", true, false),
    ];
    // 1) 目印が一致するものが最優先。
    assert_eq!(
        adopt_choice(Some("/logs/a.log"), "Implementer #1", &ws, &all),
        Some(2)
    );
    // 2) 目印が無い / 一致しないときは、同じ名前・同じフォルダ。
    assert_eq!(adopt_choice(None, "Implementer #1", &ws, &all), Some(2));
    assert_eq!(
        adopt_choice(Some("/logs/zzz.log"), "Implementer #1", &ws, &all),
        Some(2)
    );
    // 3) **既に別の担当のものは選ばない** (同じ端末を 2 体で共有しない)。
    let bound = vec![
        fact(2, "/logs/a.log", "Implementer #1", true, true),
        fact(3, "", "Implementer #2", true, false),
    ];
    assert_eq!(adopt_choice(Some("/logs/a.log"), "Implementer #1", &ws, &bound), None);
    // 4) **死んでいるものも選ばない。**
    let dead = vec![fact(2, "/logs/a.log", "Implementer #1", false, false)];
    assert_eq!(adopt_choice(Some("/logs/a.log"), "Implementer #1", &ws, &dead), None);
    // 5) フォルダが違えば名前が同じでも引き取らない。
    let elsewhere = vec![SessionFact {
        cwd: PathBuf::from("/other"),
        ..fact(2, "", "Implementer #1", true, false)
    }];
    assert_eq!(adopt_choice(None, "Implementer #1", &ws, &elsewhere), None);
    // 6) 空の目印は「目印なし」と同じ (取れなかっただけ)。
    assert_eq!(adopt_choice(Some(""), "Implementer #1", &ws, &all), Some(2));
    // 7) 候補が 1 つも無ければ起こす。
    assert_eq!(adopt_choice(Some("/logs/a.log"), "Implementer #1", &ws, &[]), None);
}

// ── C: 検証を起こした直後に落ちる ────────────────────────────────────

/// タスク `task` の完了を、いまの担当セッションから報告する。
fn report_done(rt: &mut TeamRuntime, sessions: &[SessionId], task: TaskId, now: u64) -> Vec<TeamEffect> {
    let t = rt.task(task).expect("タスク").clone();
    let session = t.assigned_session.expect("担当セッション");
    let agent = t.assigned_agent.clone().expect("担当").0;
    let cmds: Vec<String> = t
        .validation_commands
        .iter()
        .map(|c| format!("{{\"command\":\"{c}\",\"exit_code\":0}}"))
        .collect();
    let files: Vec<String> = t.files.iter().map(|x| format!("\"{x}\"")).collect();
    let report = format!(
        "{open}\n{{\"task_id\":{task},\"agent_id\":\"{agent}\",\"status\":\"completed\",\
         \"summary\":\"実装した\",\"changed_files\":[{f}],\"validation\":[{v}],\"blockers\":[]}}\n{close}",
        open = super::result_parser::RESULT_OPEN,
        close = super::result_parser::RESULT_CLOSE,
        f = files.join(","),
        v = cmds.join(","),
    );
    rt.tick(&obs_for_test_text(now, sessions, session, &report))
}

/// 完了報告まで進めて、検証の要求が出ている状態を作る。
fn awaiting_validation() -> (TeamRuntime, Vec<SessionId>, TaskId, String) {
    let (mut rt, sessions) = launched(2);
    rt.tick(&obs_for_test(2, &sessions));
    let task = rt
        .tasks()
        .iter()
        .find(|t| t.state.is_working() && t.review_of.is_none())
        .map(|t| t.id)
        .expect("実装中のタスク");
    let mut eff = report_done(&mut rt, &sessions, task, 4);
    // 検証はリポジトリのコードを走らせるので、まず人の承認を通る。
    let mut now = 5;
    let mut execution = String::new();
    for _ in 0..6 {
        approve_all(&mut rt, &eff);
        if let Some(v) = validations(&eff).into_iter().find(|v| v.task == task) {
            execution = v.execution;
            break;
        }
        eff = rt.tick(&obs_for_test(now, &sessions));
        now += 1;
    }
    assert!(!execution.is_empty(), "検証の要求が出ない");
    (rt, sessions, task, execution)
}

/// 1 セッションにだけテキストを見せる観測。
fn obs_for_test_text(
    now: u64,
    sessions: &[SessionId],
    target: SessionId,
    text: &str,
) -> Observation {
    let mut o = obs_for_test(now, sessions);
    for s in &mut o.sessions {
        if s.id == target {
            s.text = text.to_string();
        }
    }
    o
}

#[test]
fn 検証を起こした直後に落ちても永久に止まらず二重にも走らない() {
    let (mut rt, sessions, task, execution) = awaiting_validation();
    // 実行側は「裏で走らせ始めた」時点で成功を返す。
    rt.note_effect_done(&format!("validate:{execution}"));
    // 同じ刻みでもう一度は出ない。
    let again = rt.tick(&obs_for_test(5, &sessions));
    assert!(
        validations(&again).is_empty(),
        "走らせている検証をもう一度頼んだ (二重実行): {:?}",
        validations(&again)
    );

    // ★ ここで落ちる。走っていたプロセスは道連れで、結果は戻ってこない。
    let mut rt2 = crash_and_restart(&rt);
    let t = rt2.task(task).expect("タスク");
    assert_eq!(t.state, TeamTaskState::Validating, "検証待ちを捨てた");
    assert!(!t.validation.running, "走っていない検証を走っていることにした");
    // **決着していない検証の「成功」は引き継がない。** 引き継ぐと、
    // 誰も走らせていないのに記録だけが再発行を止め、永久に止まる。
    assert!(
        !rt2.effect_completed(&format!("validate:{execution}")),
        "決着していない検証を完了として引き継いだ"
    );
    let eff = rt2.tick(&obs_for_test(10, &sessions));
    assert!(
        !validations(&eff).is_empty(),
        "立て直したのに検証が撃ち直されない (永久に止まる)"
    );
}

// ── D: 記録だけ残して、外部副作用の前に落ちる ────────────────────────

#[test]
fn 発行しただけで落ちたeffectは立て直しで撃ち直される() {
    let mut rt = started_for_test(2);
    // 起動要求を出す。**実行側は何もしていない** (ACK を返していない)。
    let first = rt.tick(&obs_for_test(1, &[]));
    let issued = keys(&first);
    assert!(!issued.is_empty(), "Effect が 1 つも出ない");
    for k in &issued {
        assert!(!rt.effect_completed(k), "渡しただけで完了にした: {k}");
    }

    // ★ ここで落ちる。
    let mut rt2 = crash_and_restart(&rt);
    for k in &issued {
        assert!(
            !rt2.effect_completed(k),
            "成立していない Effect を完了として引き継いだ: {k}"
        );
    }
    let again = keys(&rt2.tick(&obs_for_test(10, &[])));
    for k in &issued {
        assert!(
            again.contains(k),
            "落ちる前に渡しただけの Effect が失われた: {k} / 出たのは {again:?}"
        );
    }
}

#[test]
fn 成立した_effectは立て直しても撃ち直さない() {
    let (mut rt, sessions) = launched(2);
    let eff = rt.tick(&obs_for_test(2, &sessions));
    let (_, key) = instructions(&eff)[0].clone();
    rt.note_effect_done(&key);

    let mut rt2 = crash_and_restart(&rt);
    assert!(
        rt2.effect_completed(&key),
        "成立した Effect の記録が消えた (もう一度実行されてしまう)"
    );
    let again = keys(&rt2.tick(&obs_for_test(10, &[])));
    assert!(
        !again.contains(&key),
        "成立済みの Effect をもう一度出した: {again:?}"
    );
}

// ── E: 古い結果が新しい試行へ混ざらない ──────────────────────────────

#[test]
fn 前の試行の検証結果は新しい試行を上書きしない() {
    let (mut rt, sessions, task, first_exec) = awaiting_validation();
    // 1 回目は落ちた。ここでタスクは担当へ差し戻され、次の回は
    // **世代が進んだ別の実行**になる。
    rt.note_validation_for(
        &first_exec,
        task,
        vec![ValidationRun::new(
            "cargo test auth",
            1,
            ValidationOutcome::Failed,
        )],
    );
    let mut now = 20;
    for _ in 0..4 {
        if rt
            .task(task)
            .is_some_and(|t| t.state.is_working() && t.assigned_session.is_some())
        {
            report_done(&mut rt, &sessions, task, now);
        } else {
            rt.tick(&obs_for_test(now, &sessions));
        }
        now += 1;
    }
    let second_exec = rt.current_execution(task);
    assert_ne!(
        second_exec, first_exec,
        "差し戻しても実行 ID が変わっていない (古い結果と区別が付かない)"
    );

    // ★ 1 回目の結果が**いま**遅れて届く (前のプロセス / 前の試行の置き土産)。
    //   しかも「成功」なので、採ってしまうと画面には「検証済み」と出るのに
    //   実際に走ったのは 1 つ前のコード、という嘘になる。
    let before = rt.task(task).map(|t| t.validation.runs.clone()).unwrap();
    rt.note_validation_for(
        &first_exec,
        task,
        vec![ValidationRun::new(
            "cargo test auth",
            0,
            ValidationOutcome::Passed,
        )],
    );
    let t = rt.task(task).expect("タスク");
    assert_eq!(
        t.validation.runs, before,
        "古い実行の結果で新しい試行の証跡を書き換えた"
    );
    assert!(
        !t.validation.runs.iter().any(|r| r.exit_code == 0),
        "古い成功を採ってしまった: {:?}",
        t.validation.runs
    );
    assert_eq!(
        rt.current_execution(task),
        second_exec,
        "古い結果が世代を進めてしまった"
    );
}

// ── 立て直しの統合: 3 通りの記録が混ざった状態から起動する ────────────

#[test]
fn 立て直しは成立済みと未成立とを取り違えない() {
    let (mut rt, sessions) = launched(2);
    let eff = rt.tick(&obs_for_test(2, &sessions));
    let sent = instructions(&eff);
    assert!(sent.len() >= 2, "2 通の指示が要る: {sent:?}");

    // 3 通りを混ぜる: 成立済み / 発行しただけ / 失敗して記録ごと外れた。
    let (_, confirmed) = sent[0].clone();
    let (_, dispatched) = sent[1].clone();
    let failed = format!(
        "start:{}",
        rt.agents()
            .iter()
            .find(|a| a.session_id.is_some())
            .expect("起動済みのエージェント")
            .id
    );
    rt.note_effect_done(&confirmed);
    rt.note_effect_failed(&failed);

    let mut rt2 = crash_and_restart(&rt);
    // 成立済み → 引き継ぐ (もう出さない)。
    assert!(rt2.effect_completed(&confirmed), "成立済みを引き継がなかった");
    // 発行しただけ → 引き継がない (もう一度出す)。
    assert!(
        !rt2.effect_completed(&dispatched),
        "発行しただけを成立済みとして引き継いだ"
    );
    // 失敗 → 記録が無い (もう一度出す)。
    assert!(!rt2.effect_completed(&failed), "失敗を成立済みにした");

    let again = keys(&rt2.tick(&obs_for_test(10, &[])));
    assert!(
        !again.contains(&confirmed),
        "成立済みの指示をもう一度出した: {again:?}"
    );
    // 起動は撃ち直される。**引き取り先つき**なので 2 体にはならない。
    let restarted = starts(&rt2.tick(&obs_for_test(11, &[])));
    let _ = restarted;
    assert!(
        again.iter().any(|k| k.starts_with("start:")),
        "エージェントを起こし直していない: {again:?}"
    );
}
