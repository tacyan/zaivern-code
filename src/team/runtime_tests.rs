//! Runtime の結合テスト — **偽エージェントで、ネットワーク無しに全経路を回す。**
//!
//! ここが「エージェントが完了と言っただけでは完了にしない」という中核の
//! 主張を実際に確かめる唯一の場所。単体テストが全部緑でも、`tick` を
//! 通して初めて分かる回帰がある (実装した本人の測定を通さないのと同じ理由で、
//! 部品の緑を全体の緑と読み替えない)。

use std::path::PathBuf;

use crate::coordinator::SessionState;

use super::model::*;
use super::planner::{PlanInput, StaticPlanner, TeamPlanner};
use super::result_parser as rp;
use super::reviewer;
use super::runtime::*;

const SPEC: &str = "\
# 認証機能

## 要件
- ログイン API を実装する (src/auth/login.rs)
- トークン更新 API を実装する (src/auth/refresh.rs)

## 完了条件
- 認証 API が動作する
- テストが成功する

## 検証
- cargo test auth
";

fn ws() -> PathBuf {
    PathBuf::from("/zaivern-team-test-workspace")
}

/// 計画から Runtime を作り、開始済みにする。
pub fn started(agents: usize) -> TeamRuntime {
    started_with(agents, true)
}

/// レビュー要否を指定して作る。
pub fn started_with(agents: usize, review_required: bool) -> TeamRuntime {
    let plan = StaticPlanner
        .plan(PlanInput {
            spec: SPEC.to_string(),
            source: "SPEC.md".into(),
            agent_count: agents,
            review_required,
            roles: Vec::new(),
        })
        .expect("計画できるべき");
    let mut rt = TeamRuntime::from_plan(
        plan,
        ws(),
        RunOptions {
            // **Run ごとに別の ID。** 実物と同じ作り方を通す
            // (固定値にすると、別 Run の結果が紛れ込む事故を再現できない)。
            run_id: new_run_id(),
            spec_source: "SPEC.md".into(),
            agent_count: agents,
            max_attempts: 3,
            review_required,
        },
    );
    rt.apply_action(TeamAction::Start);
    rt
}

fn obs(now: u64, sessions: &[(SessionId, SessionState, &str)]) -> Observation {
    Observation {
        now,
        sessions: sessions
            .iter()
            .map(|(id, st, text)| SessionObs {
                id: *id,
                title: format!("agent{id}"),
                provider: "claude".into(),
                state: *st,
                text: text.to_string(),
            })
            .collect(),
    }
}

/// 起動要求が来たセッションを結び付ける (偽の起動)。
fn bind_all(rt: &mut TeamRuntime, effects: &[TeamEffect], next: &mut SessionId) -> Vec<SessionId> {
    let mut out = Vec::new();
    for e in effects {
        if let TeamEffect::StartAgent(s) = e {
            rt.bind_session(&s.agent_id, *next);
            out.push(*next);
            *next += 1;
        }
    }
    out
}

/// 完了報告のブロックを組む。
fn result_block(task: TaskId, agent: &str, cmds: &[&str], files: &[&str]) -> String {
    let v: Vec<String> = cmds
        .iter()
        .map(|c| format!("{{\"command\":\"{c}\",\"exit_code\":0}}"))
        .collect();
    let f: Vec<String> = files.iter().map(|x| format!("\"{x}\"")).collect();
    format!(
        "{open}\n{{\"task_id\":{task},\"agent_id\":\"{agent}\",\"status\":\"completed\",\
         \"summary\":\"実装した\",\"changed_files\":[{files}],\"validation\":[{v}],\"blockers\":[]}}\n{close}",
        open = rp::RESULT_OPEN,
        close = rp::RESULT_CLOSE,
        files = f.join(","),
        v = v.join(","),
    )
}

fn review_block(target: TaskId, approve: bool) -> String {
    if approve {
        format!(
            "{open}\n{{\"task_id\":{target},\"verdict\":\"APPROVE\",\"findings\":[]}}\n{close}",
            open = reviewer::REVIEW_OPEN,
            close = reviewer::REVIEW_CLOSE
        )
    } else {
        format!(
            "{open}\n{{\"task_id\":{target},\"verdict\":\"REQUEST_CHANGES\",\
             \"findings\":[\"境界値のテストが無い\"]}}\n{close}",
            open = reviewer::REVIEW_OPEN,
            close = reviewer::REVIEW_CLOSE
        )
    }
}

/// **Zaivern 自身の検証**が成功したことにする (実行器の代わり)。
///
/// 自己申告 (`[ZAI-TEAM-RESULT]` の `validation`) は正式な証跡にならないので、
/// テストでも必ずこちらを通す — 通さずに先へ進めるなら、それは実装の欠陥。
fn validate_ok(rt: &mut TeamRuntime, tid: TaskId) {
    note_outcome(rt, tid, 0, ValidationOutcome::Passed);
}

/// いま割り当てられているタスクを (session, task_id, agent) で返す。
fn assignments(rt: &TeamRuntime) -> Vec<(SessionId, TaskId, String)> {
    let mut v: Vec<(SessionId, TaskId, String)> = rt
        .tasks()
        .iter()
        .filter(|t| t.state.is_held())
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

/// 全セッションを Idle として 1 tick 回す。
fn idle_tick(rt: &mut TeamRuntime, now: u64, sessions: &[SessionId]) -> Vec<TeamEffect> {
    let rows: Vec<(SessionId, SessionState, &str)> = sessions
        .iter()
        .map(|s| (*s, SessionState::Idle, ""))
        .collect();
    rt.tick(&obs(now, &rows))
}

/// 1 セッションにだけテキストを見せて 1 tick 回す。
///
/// **観測には必ず全セッションを載せる。** 載せ忘れたセッションは
/// 「消えた」と解釈され、担当が回収されてしまう (実際にテストで踏んだ)。
fn tick_text(
    rt: &mut TeamRuntime,
    now: u64,
    sessions: &[SessionId],
    target: SessionId,
    text: &str,
) -> Vec<TeamEffect> {
    let rows: Vec<(SessionId, SessionState, &str)> = sessions
        .iter()
        .map(|s| (*s, SessionState::Idle, if *s == target { text } else { "" }))
        .collect();
    rt.tick(&obs(now, &rows))
}

#[test]
fn 開始すると必要な数だけ起動要求が出る() {
    let mut rt = started(4);
    let eff = rt.tick(&obs(10, &[]));
    let starts: Vec<&TeamEffect> = eff
        .iter()
        .filter(|e| matches!(e, TeamEffect::StartAgent(_)))
        .collect();
    // 実装 2 本 + レビュー用の 1 体で 3 体。**4 体を無条件に起こさない。**
    assert_eq!(starts.len(), 3, "{eff:?}");
}

#[test]
fn 起動要求は二度出ない() {
    let mut rt = started(4);
    let first = rt.tick(&obs(10, &[]));
    assert!(first.iter().any(|e| matches!(e, TeamEffect::StartAgent(_))));
    let second = rt.tick(&obs(11, &[]));
    assert!(
        !second
            .iter()
            .any(|e| matches!(e, TeamEffect::StartAgent(_))),
        "同じ起動要求を再送した: {second:?}"
    );
}

#[test]
fn 依存のあるタスクは先に配られない() {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    let held: Vec<TaskId> = assignments(&rt).iter().map(|(_, t, _)| *t).collect();
    // 統合タスク (最後) は依存が残っているので配られない
    let integ = rt.tasks().last().expect("統合タスク").id;
    assert!(!held.contains(&integ), "依存未完了の統合タスクを配った");
    assert_eq!(held.len(), 2, "実装 2 本が並列に配られるべき: {held:?}");
}

#[test]
fn 同じ入力なら同じ割り当てになる() {
    let run = || {
        let mut rt = started(4);
        let e = rt.tick(&obs(10, &[]));
        let mut next = 1;
        let sids = bind_all(&mut rt, &e, &mut next);
        idle_tick(&mut rt, 11, &sids);
        assignments(&rt)
    };
    assert_eq!(run(), run());
}

#[test]
fn 完了報告だけでは完了にならずレビューへ進む() {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    let (sid, tid, agent) = assignments(&rt)[0].clone();
    let files: Vec<String> = rt.task(tid).unwrap().files.clone();
    let fs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    let block = result_block(tid, &agent, &["cargo test auth"], &fs);
    tick_text(&mut rt, 12, &sids, sid, &block);
    // **Completed にも Reviewing にもならない。** Zaivern 自身の検証待ち。
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Validating);
    assert!(
        !rt.tasks().iter().any(|t| t.review_of == Some(tid)),
        "検証前にレビュータスクを作った"
    );
    // 実測が通って初めてレビューへ進む。
    validate_ok(&mut rt, tid);
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Reviewing);
    assert!(rt.tasks().iter().any(|t| t.review_of == Some(tid)));
}

#[test]
fn 検証未実行の報告は却下される() {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    let (sid, tid, agent) = assignments(&rt)[0].clone();
    let block = result_block(tid, &agent, &[], &[]);
    tick_text(&mut rt, 12, &sids, sid, &block);
    assert_eq!(
        rt.task(tid).unwrap().state,
        TeamTaskState::Running,
        "検証なしで先へ進めてしまった"
    );
    assert!(
        rt.events().any(|e| e.kind == TeamEventKind::Rejected),
        "却下がイベントに残っていない"
    );
    // 却下理由は次の指示へ載る (黙って捨てない)
    assert!(rt
        .task(tid)
        .unwrap()
        .context
        .iter()
        .any(|c| c.contains("却下")));
}

#[test]
fn 担当外ファイルの変更は却下される() {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    let (sid, tid, agent) = assignments(&rt)[0].clone();
    let block = result_block(tid, &agent, &["cargo test auth"], &["src/other/thing.rs"]);
    tick_text(&mut rt, 12, &sids, sid, &block);
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Running);
}

#[test]
fn 他人のタスクの報告は受け取らない() {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    let a = assignments(&rt);
    let (sid_a, _, _) = a[0].clone();
    let (_, tid_b, agent_b) = a[1].clone();
    // A のセッションから B のタスクの完了を主張する
    let block = result_block(tid_b, &agent_b, &["cargo test auth"], &[]);
    tick_text(&mut rt, 12, &sids, sid_a, &block);
    assert_eq!(rt.task(tid_b).unwrap().state, TeamTaskState::Running);
}

#[test]
fn レビュアーは実装担当と別セッションになる() {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    let (sid, tid, agent) = assignments(&rt)[0].clone();
    let files: Vec<String> = rt.task(tid).unwrap().files.clone();
    let fs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    let block = result_block(tid, &agent, &["cargo test auth"], &fs);
    tick_text(&mut rt, 12, &sids, sid, &block);
    validate_ok(&mut rt, tid);
    idle_tick(&mut rt, 13, &sids);
    let rev = rt
        .tasks()
        .iter()
        .find(|t| t.review_of == Some(tid))
        .expect("レビュータスク");
    if let Some(rs) = rev.assigned_session {
        assert_ne!(rs, sid, "実装した本人がレビュー担当になった");
    }
}

#[test]
fn approveでだけ完了になる() {
    let (mut rt, sids, tid, rev_sid) = to_review_stage();
    let block = review_block(tid, true);
    tick_text(&mut rt, 20, &sids, rev_sid, &block);
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Completed);
    assert!(rt.task(tid).unwrap().review.approved());
}

#[test]
fn request_changesで差し戻される() {
    let (mut rt, sids, tid, rev_sid) = to_review_stage();
    let block = review_block(tid, false);
    tick_text(&mut rt, 20, &sids, rev_sid, &block);
    let t = rt.task(tid).unwrap();
    assert!(
        matches!(t.state, TeamTaskState::Ready | TeamTaskState::Running),
        "再実装へ戻っていない: {:?}",
        t.state
    );
    assert_ne!(t.state, TeamTaskState::Completed);
    assert_eq!(t.attempts, 1);
    assert!(
        t.context.iter().any(|c| c.contains("境界値")),
        "指摘が文脈へ載っていない: {:?}",
        t.context
    );
    // 検証はやり直す
    assert!(t.validation.runs.is_empty());
}

#[test]
fn 上限まで差し戻すと人へ上げる() {
    let (mut rt, sids, tid, mut rev_sid) = to_review_stage();
    let mut now = 20;
    for round in 0..3 {
        let block = review_block(tid, false);
        tick_text(&mut rt, now, &sids, rev_sid, &block);
        now += 1;
        if rt.task(tid).unwrap().state == TeamTaskState::NeedsUser {
            break;
        }
        // 再実装 → 再報告 → 再レビュー
        idle_tick(&mut rt, now, &sids);
        now += 1;
        let Some((sid, _, agent)) = assignments(&rt).into_iter().find(|(_, t, _)| *t == tid) else {
            panic!("再割り当てされていない (round {round})");
        };
        let files: Vec<String> = rt.task(tid).unwrap().files.clone();
        let fs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
        let block = result_block(tid, &agent, &["cargo test auth"], &fs);
        tick_text(&mut rt, now, &sids, sid, &block);
        now += 1;
        validate_ok(&mut rt, tid);
        idle_tick(&mut rt, now, &sids);
        now += 1;
        rev_sid = rt
            .tasks()
            .iter()
            .find(|t| t.review_of == Some(tid) && t.state.is_held())
            .and_then(|t| t.assigned_session)
            .unwrap_or(rev_sid);
    }
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::NeedsUser);
    assert!(
        rt.decisions()
            .iter()
            .any(|d| d.kind == DecisionKind::AttemptsExhausted),
        "人へ上げていない: {:?}",
        rt.decisions()
    );
}

#[test]
fn 一時停止中は新規割り当てをしないが状態は更新する() {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    rt.apply_action(TeamAction::Pause);
    let before = assignments(&rt);
    idle_tick(&mut rt, 11, &sids);
    assert_eq!(assignments(&rt), before, "一時停止中に配ってしまった");
    // 状態更新は続く (Idle として観測されている)
    assert!(rt
        .agents()
        .iter()
        .any(|a| a.session_id.is_some() && a.state != AgentWorkState::Unknown));
    // 再開すれば配る
    rt.apply_action(TeamAction::Resume);
    idle_tick(&mut rt, 12, &sids);
    assert!(!assignments(&rt).is_empty());
}

#[test]
fn stopは承認ゲートを通す() {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    let eff = rt.apply_action(TeamAction::Stop);
    // **いきなり kill しない。** 承認要求が出る。
    assert!(
        eff.iter()
            .any(|e| matches!(e, TeamEffect::RequestHumanApproval(_))),
        "{eff:?}"
    );
    assert!(
        !eff.iter().any(|e| matches!(e, TeamEffect::StopAgent(_))),
        "承認前に kill した"
    );
    let id = rt.decisions()[0].id;
    let eff2 = rt.apply_action(TeamAction::ApproveDecision(id));
    assert_eq!(
        eff2.iter()
            .filter(|e| matches!(e, TeamEffect::StopAgent(_)))
            .count(),
        sids.len()
    );
}

#[test]
fn セッションが消えたら担当を回収する() {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    let (dead, tid, _) = assignments(&rt)[0].clone();
    let alive: Vec<SessionId> = sids.iter().copied().filter(|s| *s != dead).collect();
    idle_tick(&mut rt, 12, &alive);
    let t = rt.task(tid).unwrap();
    // 消えたセッションが担当のまま残らないこと。同じ tick で生きている
    // 別のセッションへ配り直されるのは正しい (前任の停止は確認済み)。
    assert_ne!(t.assigned_session, Some(dead), "死んだ担当が残っている");
    assert!(t.attempts >= 1, "回収を試行回数に数えていない");
    assert!(
        !t.state.is_terminal() || t.state == TeamTaskState::NeedsUser,
        "勝手に完了させた: {:?}",
        t.state
    );
}

#[test]
fn 偽のサブエージェントイベントは受け取らない() {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    let before = rt.agents().len();
    // 親が実在しない
    let bad = format!(
        "{open}\n{{\"kind\":\"sub_agent_started\",\"agent_id\":\"ghost\",\"parent_id\":\"nobody\"}}\n{close}",
        open = rp::EVENT_OPEN,
        close = rp::EVENT_CLOSE
    );
    tick_text(&mut rt, 12, &sids, sids[0], &bad);
    assert_eq!(rt.agents().len(), before, "存在しない親の下に生やした");
    // 画面の地の文からは何も作らない
    tick_text(&mut rt, 13, &sids, sids[0], "sub_agent_started ghost2\n");
    assert_eq!(rt.agents().len(), before);
}

#[test]
fn 報告されたサブエージェントは端末を開けない() {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    let parent = rt
        .agents()
        .iter()
        .find(|a| a.session_id == Some(sids[0]))
        .map(|a| a.id.clone())
        .expect("親");
    let ev = format!(
        "{open}\n{{\"kind\":\"sub_agent_started\",\"agent_id\":\"backend-test-1\",\
         \"parent_id\":\"{parent}\",\"role\":\"tester\",\"action\":\"テスト作成中\"}}\n{close}",
        open = rp::EVENT_OPEN,
        close = rp::EVENT_CLOSE
    );
    tick_text(&mut rt, 12, &sids, sids[0], &ev);
    let sub = rt
        .agent(&AgentId::new("backend-test-1"))
        .expect("サブエージェントが登録されるべき");
    assert_eq!(sub.kind, AgentKind::ReportedSubAgent);
    assert!(
        !sub.can_open_terminal(),
        "開けない端末のボタンを出してしまう"
    );
    assert_eq!(sub.parent_id, Some(parent));
}

#[test]
fn エージェント0体でもpanicしない() {
    let mut rt = started(1);
    for now in 0..5 {
        let _ = rt.tick(&obs(now, &[]));
    }
    assert!(!rt.tasks().is_empty());
}

#[test]
fn 不正なspecでもpanicしない() {
    let bad = StaticPlanner.plan(PlanInput {
        spec: String::new(),
        source: "SPEC.md".into(),
        agent_count: 4,
        review_required: true,
        roles: Vec::new(),
    });
    assert!(bad.is_err());
    // 制御文字だらけの SPEC でも計画は作れるか、Err になるだけ
    let weird = StaticPlanner.plan(PlanInput {
        spec: "\u{0}\u{1}#\n- \u{7}\n".into(),
        source: "x".into(),
        agent_count: 1,
        review_required: true,
        roles: Vec::new(),
    });
    assert!(weird.is_ok() || weird.is_err());
}

#[test]
fn 保存して復元しても実行中タスクを勝手に走らせない() {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    assert!(!assignments(&rt).is_empty());
    let saved = rt.to_saved();
    let restored = TeamRuntime::restore(saved, ws());
    // **プロセス生存を確認できていないので、Running のままにしない。**
    assert!(assignments(&restored).is_empty());
    for t in restored.tasks() {
        assert!(!t.state.is_held(), "#{} が {:?} のまま", t.id, t.state);
        assert_eq!(t.assigned_session, None);
    }
    for a in restored.agents() {
        assert_eq!(a.session_id, None);
    }
}

#[test]
fn 起動済みのエージェントへ同じ起動要求を撃ち直さない() {
    // 同じプロセスの中では、ACK が返ってセッションが結び付いた時点で完了。
    let mut rt = started(4);
    let first = rt.tick(&obs(10, &[]));
    assert!(first.iter().any(|e| matches!(e, TeamEffect::StartAgent(_))));
    let mut next = 1;
    let sids = bind_all(&mut rt, &first, &mut next);
    for e in &first {
        let k = e.key();
        if !k.is_empty() {
            rt.note_effect_done(&k);
        }
    }
    let again = idle_tick(&mut rt, 11, &sids);
    assert!(
        !again.iter().any(|e| matches!(e, TeamEffect::StartAgent(_))),
        "起動済みなのに撃ち直した: {again:?}"
    );
}

#[test]
fn 再起動後はackが返っていてもエージェントを起こし直す() {
    // **`StartAgent` の成果は「生きているセッション」**なので、再起動すれば
    // 子プロセスは死んでいる。ACK の記録だけを見て「済んだ」と判断すると、
    // 復元後にエージェントが 1 体も居ないまま止まる。
    let mut rt = started(4);
    let first = rt.tick(&obs(10, &[]));
    let mut next = 1;
    bind_all(&mut rt, &first, &mut next);
    for e in &first {
        let k = e.key();
        if !k.is_empty() {
            rt.note_effect_done(&k);
        }
    }
    let saved = rt.to_saved();
    let mut r = TeamRuntime::restore(saved, ws());
    r.apply_action(TeamAction::Start);
    let after = r.tick(&obs(11, &[]));
    assert!(
        after.iter().any(|e| matches!(e, TeamEffect::StartAgent(_))),
        "再起動後にエージェントを起こし直さない: {after:?}"
    );
}

#[test]
fn イベントは上限を超えない() {
    let mut rt = started(4);
    for i in 0..(EVENT_CAP as u64 + 200) {
        rt.apply_action(TeamAction::AddContext {
            task: 1,
            text: format!("x{i}"),
        });
        // AddContext はイベントを積まないので、却下イベントで埋める
        rt.tick(&obs(i, &[]));
    }
    assert!(rt.events().count() <= EVENT_CAP);
}

/// レビュー待ちまで進めた状態を作る。
/// 返すのは (runtime, 全セッション, 実装タスク ID, レビュー担当セッション)。
fn to_review_stage() -> (TeamRuntime, Vec<SessionId>, TaskId, SessionId) {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    let (sid, tid, agent) = assignments(&rt)[0].clone();
    let files: Vec<String> = rt.task(tid).unwrap().files.clone();
    let fs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    let block = result_block(tid, &agent, &["cargo test auth"], &fs);
    tick_text(&mut rt, 12, &sids, sid, &block);
    // **Zaivern 自身の検証が通って初めてレビューへ進む。**
    validate_ok(&mut rt, tid);
    idle_tick(&mut rt, 13, &sids);
    let rev_sid = rt
        .tasks()
        .iter()
        .find(|t| t.review_of == Some(tid))
        .and_then(|t| t.assigned_session)
        .unwrap_or_else(|| *sids.iter().find(|s| **s != sid).unwrap_or(&sid));
    (rt, sids, tid, rev_sid)
}

#[test]
fn 同じエージェントを複数チームへ重複登録しない() {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    let parent = rt
        .agents()
        .iter()
        .find(|a| a.session_id == Some(sids[0]))
        .map(|a| a.id.clone())
        .expect("親");
    let other = rt
        .agents()
        .iter()
        .find(|a| a.session_id == Some(sids[1]))
        .map(|a| a.id.clone())
        .expect("別の親");
    let ev = |parent: &AgentId| {
        format!(
            "{open}\n{{\"kind\":\"sub_agent_started\",\"agent_id\":\"dup-1\",\
             \"parent_id\":\"{parent}\",\"role\":\"tester\"}}\n{close}",
            open = rp::EVENT_OPEN,
            close = rp::EVENT_CLOSE
        )
    };
    tick_text(&mut rt, 12, &sids, sids[0], &ev(&parent));
    let after_first = rt.agents().len();
    // **別の親の下に同じ ID を作らせない。** 通すと組織図に同じ名前が
    // 2 つ現れ、どちらが本物か分からなくなる。
    tick_text(&mut rt, 13, &sids, sids[1], &ev(&other));
    assert_eq!(rt.agents().len(), after_first, "同じ ID を二重登録した");
    let dup_parent = rt
        .agent(&AgentId::new("dup-1"))
        .and_then(|a| a.parent_id.clone())
        .expect("最初の登録は残る");
    assert_eq!(dup_parent, parent, "親が乗っ取られた");
    // 同じ親からの再報告は受け入れる (進捗の更新なので)
    tick_text(&mut rt, 14, &sids, sids[0], &ev(&dup_parent));
    assert_eq!(rt.agents().len(), after_first);
}

#[test]
fn specとエージェント数が計画に反映される() {
    // `zai team run SPEC.md --agents 4` と GUI の New Team Run は
    // **同じ Planner と同じ Runtime** を通る。ここではその計画に SPEC の
    // 中身と指定したエージェント数が効いていることを見る。
    for agents in [1usize, 2, 4] {
        let rt = {
            let plan = StaticPlanner
                .plan(PlanInput {
                    spec: SPEC.to_string(),
                    source: "SPEC.md".into(),
                    agent_count: agents,
                    review_required: true,
                    roles: Vec::new(),
                })
                .expect("計画できるべき");
            TeamRuntime::from_plan(
                plan,
                ws(),
                RunOptions {
                    agent_count: agents,
                    ..RunOptions::default()
                },
            )
        };
        assert_eq!(rt.run().agent_count, agents);
        assert_eq!(rt.goal().title, "認証機能");
        assert!(
            rt.goal().specification.contains("トークン更新"),
            "SPEC の中身が計画へ入っていない"
        );
        // 起こすのは「計画に必要なぶんだけ」で、上限は超えない
        let roster = rt
            .agents()
            .iter()
            .filter(|a| a.kind == AgentKind::ManagedSession)
            .count();
        assert!(roster <= agents, "agents={agents} なのに {roster} 体並べた");
        assert!(roster >= 1);
    }
}

#[test]
fn 計画しただけではエージェントを起こさない() {
    // Plan Preview は「見せるだけ」。Start Team を押すまで 1 体も起きない。
    let plan = StaticPlanner
        .plan(PlanInput {
            spec: SPEC.to_string(),
            source: "SPEC.md".into(),
            agent_count: 4,
            review_required: true,
            roles: Vec::new(),
        })
        .unwrap();
    let mut rt = TeamRuntime::from_plan(plan, ws(), RunOptions::default());
    assert_eq!(rt.goal().status, GoalStatus::Ready);
    let eff = rt.tick(&obs(1, &[]));
    assert!(
        !eff.iter().any(|e| matches!(e, TeamEffect::StartAgent(_))),
        "Start Team を押す前に起動要求が出た: {eff:?}"
    );
    rt.apply_action(TeamAction::Start);
    let eff2 = rt.tick(&obs(2, &[]));
    assert!(eff2.iter().any(|e| matches!(e, TeamEffect::StartAgent(_))));
}

// ══════════════════════════════════════════════════════════════════════
//  修正 1: 検証結果の信頼境界
//
//  **エージェントの自己申告を正式な検証証跡にしない。** Zaivern 自身が
//  実行し、`note_validation` で戻ってきた実測結果だけを採る。
// ══════════════════════════════════════════════════════════════════════

/// 完了報告を出した直後の状態と、そのとき出た Effect。
fn report_and_collect(rt: &mut TeamRuntime, sids: &[SessionId], tid: TaskId, now: u64)
    -> Vec<TeamEffect>
{
    let t = rt.task(tid).expect("タスク").clone();
    let sid = t.assigned_session.expect("担当セッション");
    let agent = t.assigned_agent.clone().expect("担当").0;
    let files: Vec<String> = t.files.clone();
    let fs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    let cmds: Vec<&str> = t.validation_commands.iter().map(|s| s.as_str()).collect();
    let block = result_block(tid, &agent, &cmds, &fs);
    let rows: Vec<(SessionId, SessionState, &str)> = sids
        .iter()
        .map(|s| (*s, SessionState::Idle, if *s == sid { block.as_str() } else { "" }))
        .collect();
    rt.tick(&obs(now, &rows))
}

/// 報告 → **人が実行を承認** → 検証が発行される、まで進める。
fn report_approve_and_collect(
    rt: &mut TeamRuntime,
    sids: &[SessionId],
    tid: TaskId,
    now: u64,
) -> Vec<TeamEffect> {
    report_and_collect(rt, sids, tid, now);
    approve_validation(rt);
    idle_tick(rt, now + 1, sids)
}

/// 保留中の「検証の実行許可」を人が承認する。
///
/// `cargo test` はリポジトリ内のコードを実行するので、**実物でも人が 1 度
/// 承認しないと 1 行も走らない**。テストもその経路を通す (迂回する近道を
/// 作らない)。承認した件数を返す。
fn approve_validation(rt: &mut TeamRuntime) -> usize {
    let ids: Vec<u64> = rt
        .decisions()
        .iter()
        .filter(|d| d.kind == DecisionKind::ValidationExecution)
        .map(|d| d.id)
        .collect();
    for id in &ids {
        rt.apply_action(TeamAction::ApproveDecision(*id));
    }
    ids.len()
}

/// 実装 1 本を割り当てた状態を作る。
fn to_assigned() -> (TeamRuntime, Vec<SessionId>, TaskId) {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    let tid = assignments(&rt)[0].1;
    (rt, sids, tid)
}

#[test]
fn 自己申告の検証成功でもzaivernが自分で実行する() {
    let (mut rt, sids, tid) = to_assigned();
    // エージェントは "cargo test auth / exit_code=0" を自己申告する。
    // **実行はリポジトリのコードを走らせるので、人の承認を通る。**
    let eff = report_approve_and_collect(&mut rt, &sids, tid, 12);
    // **Zaivern 自身の検証が必ず発行される。**
    assert!(
        eff.iter().any(|e| matches!(e, TeamEffect::RunValidation(v) if v.task == tid)),
        "自己申告を信じて検証を実行しなかった: {eff:?}"
    );
    // まだレビューへは進まない
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Validating);
    assert!(
        !rt.tasks().iter().any(|t| t.review_of == Some(tid)),
        "検証前にレビュータスクを作った"
    );
    // 正式な検証証跡は 1 件も無い (自己申告は入っていない)
    assert!(
        rt.task(tid).unwrap().validation.runs.is_empty(),
        "自己申告が正式な検証結果として入っている: {:?}",
        rt.task(tid).unwrap().validation.runs
    );
}

#[test]
fn 実測が失敗ならレビューへ進まない() {
    let (mut rt, sids, tid) = to_assigned();
    report_and_collect(&mut rt, &sids, tid, 12);
    approve_validation(&mut rt);
    idle_tick(&mut rt, 12, &sids);
    // 実測は失敗
    note_outcome(&mut rt, tid, 1, ValidationOutcome::Failed);
    idle_tick(&mut rt, 13, &sids);
    let t = rt.task(tid).unwrap();
    assert_ne!(t.state, TeamTaskState::Reviewing, "失敗したのにレビューへ進んだ");
    assert_ne!(t.state, TeamTaskState::Completed, "失敗したのに完了した");
    assert!(
        !rt.tasks().iter().any(|x| x.review_of == Some(tid)),
        "失敗したのにレビュータスクを作った"
    );
}

#[test]
fn 実測が成功して初めてレビューへ進む() {
    let (mut rt, sids, tid) = to_assigned();
    report_and_collect(&mut rt, &sids, tid, 12);
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Validating);
    approve_validation(&mut rt);
    idle_tick(&mut rt, 12, &sids);
    validate_ok(&mut rt, tid);
    let t = rt.task(tid).unwrap();
    assert_eq!(t.state, TeamTaskState::Reviewing, "実測成功後にレビューへ進まない");
    assert!(rt.tasks().iter().any(|x| x.review_of == Some(tid)));
}

#[test]
fn レビュー不要でも実測成功前は完了しない() {
    let mut rt = started_with(4, false);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    let tid = assignments(&rt)[0].1;
    report_and_collect(&mut rt, &sids, tid, 12);
    assert_ne!(
        rt.task(tid).unwrap().state,
        TeamTaskState::Completed,
        "レビュー不要でも自己申告だけで完了させてはいけない"
    );
    approve_validation(&mut rt);
    idle_tick(&mut rt, 12, &sids);
    validate_ok(&mut rt, tid);
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Completed);
}

#[test]
fn 検証コマンドが複数なら全部成功しないと進まない() {
    let (mut rt, sids, tid) = to_assigned();
    rt.set_validation_commands_for_test(tid, &["cargo test a", "cargo test b"]);
    report_and_collect(&mut rt, &sids, tid, 12);
    approve_validation(&mut rt);
    idle_tick(&mut rt, 12, &sids);
    // 1 本だけ成功
    let exec = rt.current_execution(tid);
    rt.note_validation_for(&exec, tid, vec![ValidationRun::passed("cargo test a")]);
    assert_eq!(
        rt.task(tid).unwrap().state,
        TeamTaskState::Validating,
        "一部だけの成功で先へ進んだ"
    );
    let exec = rt.current_execution(tid);
    rt.note_validation_for(&exec, tid, vec![ValidationRun::passed("cargo test b")]);
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Reviewing);
}

#[test]
fn 自己申告と実測が矛盾したら実測を採る() {
    let (mut rt, sids, tid) = to_assigned();
    // 自己申告は exit_code 0
    report_and_collect(&mut rt, &sids, tid, 12);
    approve_validation(&mut rt);
    idle_tick(&mut rt, 12, &sids);
    // 実測は 1
    note_outcome(&mut rt, tid, 1, ValidationOutcome::Failed);
    let t = rt.task(tid).unwrap();
    assert!(t.validation.failed(), "実測の失敗が採られていない");
    assert!(!t.validation.passed(&t.validation_commands));
    assert_ne!(t.state, TeamTaskState::Reviewing);
}

#[test]
fn 検証コマンドが空でも止まらない() {
    let (mut rt, sids, tid) = to_assigned();
    rt.set_validation_commands_for_test(tid, &[]);
    report_and_collect(&mut rt, &sids, tid, 12);
    // 空 = 走らせるものが無い。**永久に Validating で止めない。**
    let st = rt.task(tid).unwrap().state;
    assert!(
        matches!(st, TeamTaskState::Reviewing | TeamTaskState::Completed),
        "検証コマンドが空のタスクが {st:?} で止まっている"
    );
}

#[test]
fn 再起動しても未完了の検証が再開される() {
    let (mut rt, sids, tid) = to_assigned();
    report_approve_and_collect(&mut rt, &sids, tid, 12);
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Validating);
    let saved = rt.to_saved();
    let mut r = TeamRuntime::restore(saved, ws());
    r.apply_action(TeamAction::Start);
    let eff = r.tick(&obs(20, &[]));
    assert!(
        eff.iter().any(|e| matches!(e, TeamEffect::RunValidation(_))),
        "復元後に未完了の検証が再開されない: {eff:?}"
    );
    let _ = sids;
}

// ══════════════════════════════════════════════════════════════════════
//  検証は必ず決着する — 時間切れ・停止・接続断・古い結果
// ══════════════════════════════════════════════════════════════════════

/// 実行 ID を添えて、指定した終わり方の実測を戻す (偽の実行器)。
fn note_outcome(rt: &mut TeamRuntime, tid: TaskId, code: i32, out: ValidationOutcome) {
    let exec = rt.current_execution(tid);
    let cmds = rt
        .task(tid)
        .map(|t| t.validation_commands.clone())
        .unwrap_or_default();
    let runs = cmds
        .iter()
        .map(|c| ValidationRun::new(c, code, out))
        .collect();
    rt.note_validation_for(&exec, tid, runs);
}

#[test]
fn 時間切れは失敗として決着する() {
    // **`Validating` に残さない。** 残すと Team Run 全体が静かに止まる。
    let (mut rt, sids, tid) = to_assigned();
    report_approve_and_collect(&mut rt, &sids, tid, 12);
    assert!(rt.task(tid).unwrap().validation.running);
    note_outcome(&mut rt, tid, 124, ValidationOutcome::TimedOut);
    let t = rt.task(tid).unwrap();
    assert!(!t.validation.running, "時間切れなのに実行中のまま");
    assert_ne!(t.state, TeamTaskState::Validating, "永久に Validating に残った");
    assert_ne!(t.state, TeamTaskState::Reviewing, "落ちたのに先へ進めた");
    // 理由が次の担当へ伝わる。
    assert!(
        t.context.iter().any(|c| c.contains("時間切れ")),
        "時間切れだと分かる形で残していない: {:?}",
        t.context
    );
}

#[test]
fn 接続断も起動失敗も決着する() {
    for (code, out) in [
        (125, ValidationOutcome::RunnerDisconnected),
        (126, ValidationOutcome::SpawnFailed),
    ] {
        let (mut rt, sids, tid) = to_assigned();
        report_approve_and_collect(&mut rt, &sids, tid, 12);
        note_outcome(&mut rt, tid, code, out);
        let t = rt.task(tid).unwrap();
        assert!(!t.validation.running, "{out:?} で実行中のまま");
        assert_ne!(t.state, TeamTaskState::Validating, "{out:?} で止まった");
        assert_ne!(t.state, TeamTaskState::Reviewing, "{out:?} で先へ進めた");
    }
}

#[test]
fn 停止による打ち切りは失敗として数えない() {
    // 人が止めたのを「実装が悪い」と読み替えない。決着 (running=false) は
    // つくので永久には止まらず、再開すればもう一度走る。
    let (mut rt, sids, tid) = to_assigned();
    report_approve_and_collect(&mut rt, &sids, tid, 12);
    let before = rt.task(tid).unwrap().attempts;
    note_outcome(&mut rt, tid, 130, ValidationOutcome::Cancelled);
    let t = rt.task(tid).unwrap();
    assert!(!t.validation.running, "打ち切ったのに実行中のまま");
    assert_eq!(t.attempts, before, "人が止めたぶんを失敗として数えた");
    assert_ne!(t.state, TeamTaskState::Reviewing, "打ち切ったのに先へ進めた");
    // 再開すればもう一度発行される。
    let eff = idle_tick(&mut rt, 20, &sids);
    assert!(
        eff.iter()
            .any(|e| matches!(e, TeamEffect::RunValidation(v) if v.task == tid)),
        "打ち切った検証が再開されない: {eff:?}"
    );
}

#[test]
fn 古い実行の結果は採用しない() {
    // 差し戻して配り直した後に、前の実行の結果が遅れて届く筋書き。
    let (mut rt, sids, tid) = to_assigned();
    report_approve_and_collect(&mut rt, &sids, tid, 12);
    let stale = rt.current_execution(tid);
    // 検証が落ちて差し戻される (試行が 1 つ進む)。
    note_outcome(&mut rt, tid, 1, ValidationOutcome::Failed);
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Ready);
    // ここで**古い実行**が「成功しました」と戻ってくる。
    let cmds = rt.task(tid).unwrap().validation_commands.clone();
    rt.note_validation_for(
        &stale,
        tid,
        cmds.iter().map(ValidationRun::passed).collect(),
    );
    let t = rt.task(tid).unwrap();
    assert_ne!(t.state, TeamTaskState::Reviewing, "古い結果で先へ進めた");
    assert!(
        !t.validation.passed(&t.validation_commands),
        "古い結果を正式な証跡として採った: {:?}",
        t.validation.runs
    );
}

#[test]
fn 別のrunの検証結果は同じタスク番号でも採らない() {
    // タスク ID は Run ごとに 1 から振り直される。`run_id` を実行 ID へ
    // 入れていないと、前の Run の結果が新しい Run の同じ番号のタスクへ
    // 適用される (置き場はワークスペース単位なので実際に隣り合う)。
    let (mut old, sids_a, tid) = to_assigned();
    report_approve_and_collect(&mut old, &sids_a, tid, 12);
    let from_old_run = old.current_execution(tid);

    let (mut fresh, sids_b, tid2) = to_assigned();
    assert_eq!(tid, tid2, "同じ番号のタスクで比べる前提が崩れた");
    report_approve_and_collect(&mut fresh, &sids_b, tid2, 12);
    let cmds = fresh.task(tid2).unwrap().validation_commands.clone();
    fresh.note_validation_for(
        &from_old_run,
        tid2,
        cmds.iter().map(ValidationRun::passed).collect(),
    );
    let t = fresh.task(tid2).unwrap();
    assert!(
        !t.validation.passed(&t.validation_commands),
        "別の Run の結果を採った: {:?}",
        t.validation.runs
    );
    assert_ne!(t.state, TeamTaskState::Reviewing);
}

#[test]
fn stopを承認すると走っている検証も止める() {
    let (mut rt, sids, tid) = to_assigned();
    report_approve_and_collect(&mut rt, &sids, tid, 12);
    assert!(rt.task(tid).unwrap().validation.running);
    // **承認前は止めない。**
    let e1 = rt.apply_action(TeamAction::Stop);
    assert!(
        !e1.iter()
            .any(|e| matches!(e, TeamEffect::CancelValidation { .. })),
        "承認前に検証を止めた: {e1:?}"
    );
    let did = rt
        .decisions()
        .iter()
        .find(|d| d.kind == DecisionKind::StopAgents)
        .expect("停止承認")
        .id;
    let e2 = rt.apply_action(TeamAction::ApproveDecision(did));
    assert!(
        e2.iter()
            .any(|e| matches!(e, TeamEffect::CancelValidation { task, .. } if *task == tid)),
        "承認したのに検証を止めない: {e2:?}"
    );
    let _ = sids;
}

#[test]
fn 止めたrunは遅れて届いた成功でも完了しない() {
    let (mut rt, sids, tid) = to_assigned();
    report_approve_and_collect(&mut rt, &sids, tid, 12);
    rt.apply_action(TeamAction::Stop);
    // 停止のあとで、走っていた検証が「成功」と戻ってくる。
    note_outcome(&mut rt, tid, 0, ValidationOutcome::Passed);
    idle_tick(&mut rt, 30, &sids);
    assert_ne!(
        rt.goal().status,
        GoalStatus::Completed,
        "止めた Run が完了した"
    );
}

#[test]
fn 一時停止中は新しい検証を始めない() {
    // 検証はリポジトリのコードを走らせる「仕事」なので、Pause の対象。
    let (mut rt, sids, tid) = to_assigned();
    report_and_collect(&mut rt, &sids, tid, 12);
    approve_validation(&mut rt);
    rt.apply_action(TeamAction::Pause);
    let eff = idle_tick(&mut rt, 13, &sids);
    assert!(
        !eff.iter()
            .any(|e| matches!(e, TeamEffect::RunValidation(_))),
        "一時停止中に検証を始めた: {eff:?}"
    );
    rt.apply_action(TeamAction::Resume);
    let eff = idle_tick(&mut rt, 14, &sids);
    assert!(
        eff.iter()
            .any(|e| matches!(e, TeamEffect::RunValidation(v) if v.task == tid)),
        "再開しても検証が始まらない: {eff:?}"
    );
}

// ══════════════════════════════════════════════════════════════════════
//  修正 2: Reassign は旧担当の停止を確認してから
// ══════════════════════════════════════════════════════════════════════

#[test]
fn 実行中のreassignは即座にreadyにしない() {
    let (mut rt, _sids, tid) = to_assigned();
    let before = rt.task(tid).unwrap().clone();
    assert!(before.assigned_session.is_some());
    let eff = rt.apply_action(TeamAction::ReassignTask(tid));
    let t = rt.task(tid).unwrap();
    assert_ne!(t.state, TeamTaskState::Ready, "旧担当が生きているのに Ready へ戻した");
    assert_eq!(t.assigned_session, before.assigned_session, "担当を先に外した");
    // 承認前に kill しない
    assert!(
        !eff.iter().any(|e| matches!(e, TeamEffect::StopAgent(_))),
        "承認前に StopAgent を発行した: {eff:?}"
    );
    // 承認要求は出る
    assert!(
        eff.iter().any(|e| matches!(e, TeamEffect::RequestHumanApproval(_))),
        "停止承認を求めていない: {eff:?}"
    );
}

#[test]
fn reassignは承認後にstop_agentを出す() {
    let (mut rt, _sids, tid) = to_assigned();
    let sid = rt.task(tid).unwrap().assigned_session.unwrap();
    rt.apply_action(TeamAction::ReassignTask(tid));
    let d = rt
        .decisions()
        .iter()
        .find(|d| d.task_id == Some(tid))
        .expect("停止承認の判断が積まれる")
        .clone();
    let eff = rt.apply_action(TeamAction::ApproveDecision(d.id));
    assert!(
        eff.iter().any(|e| matches!(e, TeamEffect::StopAgent(s) if *s == sid)),
        "承認後に StopAgent が出ない: {eff:?}"
    );
}

#[test]
fn セッションが生きている間は別担当へ配らない() {
    let (mut rt, sids, tid) = to_assigned();
    let sid = rt.task(tid).unwrap().assigned_session.unwrap();
    rt.apply_action(TeamAction::ReassignTask(tid));
    let d = rt.decisions().iter().find(|d| d.task_id == Some(tid)).unwrap().id;
    rt.apply_action(TeamAction::ApproveDecision(d));
    // セッションはまだ生きている
    idle_tick(&mut rt, 13, &sids);
    let t = rt.task(tid).unwrap();
    assert_eq!(
        t.assigned_session,
        Some(sid),
        "セッションが生きているのに担当を外した"
    );
    // 消滅を観測して初めて解放される
    let alive: Vec<SessionId> = sids.iter().copied().filter(|s| *s != sid).collect();
    idle_tick(&mut rt, 14, &alive);
    let t = rt.task(tid).unwrap();
    assert_ne!(t.assigned_session, Some(sid), "消滅後も旧担当が残っている");
}

#[test]
fn reassignを拒否したら元の担当と状態が残る() {
    let (mut rt, _sids, tid) = to_assigned();
    let before = rt.task(tid).unwrap().clone();
    rt.apply_action(TeamAction::ReassignTask(tid));
    let d = rt.decisions().iter().find(|d| d.task_id == Some(tid)).unwrap().id;
    rt.apply_action(TeamAction::RejectDecision(d));
    let after = rt.task(tid).unwrap();
    assert_eq!(after.state, before.state, "拒否したのに状態が変わった");
    assert_eq!(after.assigned_session, before.assigned_session);
    assert_eq!(after.assigned_agent, before.assigned_agent);
}

#[test]
fn reassignの途中で再起動しても継続できる() {
    let (mut rt, _sids, tid) = to_assigned();
    rt.apply_action(TeamAction::ReassignTask(tid));
    let saved = rt.to_saved();
    let r = TeamRuntime::restore(saved, ws());
    assert!(
        r.decisions().iter().any(|d| d.task_id == Some(tid)),
        "停止承認待ちが復元されない"
    );
}

#[test]
fn 旧担当が居なければ即座に回収できる() {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    // 依存待ちの統合タスク (担当が居ない)
    let integ = rt
        .tasks()
        .iter()
        .find(|t| t.role == TeamRole::Integrator)
        .map(|t| t.id)
        .expect("統合タスク");
    assert_eq!(rt.task(integ).unwrap().assigned_session, None);
    let eff = rt.apply_action(TeamAction::ReassignTask(integ));
    assert!(
        !eff.iter().any(|e| matches!(e, TeamEffect::RequestHumanApproval(_))),
        "担当が居ないのに承認を求めた"
    );
    assert!(rt.decisions().iter().all(|d| d.task_id != Some(integ)));
}

// ══════════════════════════════════════════════════════════════════════
//  修正 3: Effect は実行前に「完了済み」と記録しない
// ══════════════════════════════════════════════════════════════════════

// ══════════════════════════════════════════════════════════════════════
//  検証コマンドの実行は、リスクに応じて人の承認を通す
//
//  `cargo test` にシェルのメタ文字は 1 つも無いが、build.rs / テスト本体 /
//  conftest.py / Makefile を通じて **リポジトリ内の任意コードを実行できる**。
//  隔離が無い以上「許可リストに載っているから安全」とは言えない。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn リポジトリコードを実行する検証は承認前に走らない() {
    let (mut rt, sids, tid) = to_assigned();
    let eff = report_and_collect(&mut rt, &sids, tid, 12);
    // **RunValidation は出ない。** 代わりに承認を求める。
    assert!(
        !eff.iter()
            .any(|e| matches!(e, TeamEffect::RunValidation(_))),
        "承認前に検証を実行しようとした: {eff:?}"
    );
    let d = rt
        .decisions()
        .iter()
        .find(|d| d.kind == DecisionKind::ValidationExecution)
        .expect("実行許可を求めていない");
    assert!(
        d.commands.iter().any(|c| c.contains("cargo test")),
        "何を実行するのかを判断へ載せていない: {:?}",
        d.commands
    );
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Validating);
    // 承認しない限り、何度回しても出ない。
    let again = idle_tick(&mut rt, 13, &sids);
    assert!(!again
        .iter()
        .any(|e| matches!(e, TeamEffect::RunValidation(_))));
}

#[test]
fn 承認したあとにだけ検証が発行される() {
    let (mut rt, sids, tid) = to_assigned();
    report_and_collect(&mut rt, &sids, tid, 12);
    let did = rt
        .decisions()
        .iter()
        .find(|d| d.kind == DecisionKind::ValidationExecution)
        .expect("実行許可")
        .id;
    rt.apply_action(TeamAction::ApproveDecision(did));
    let eff = idle_tick(&mut rt, 13, &sids);
    assert!(
        eff.iter()
            .any(|e| matches!(e, TeamEffect::RunValidation(v) if v.task == tid)),
        "承認したのに検証が発行されない: {eff:?}"
    );
}

#[test]
fn 承認を拒否したら実行せず人へ上げる() {
    let (mut rt, sids, tid) = to_assigned();
    report_and_collect(&mut rt, &sids, tid, 12);
    let did = rt
        .decisions()
        .iter()
        .find(|d| d.kind == DecisionKind::ValidationExecution)
        .expect("実行許可")
        .id;
    rt.apply_action(TeamAction::RejectDecision(did));
    let eff = idle_tick(&mut rt, 13, &sids);
    assert!(
        !eff.iter()
            .any(|e| matches!(e, TeamEffect::RunValidation(_))),
        "拒否したのに実行した: {eff:?}"
    );
    assert_eq!(
        rt.task(tid).unwrap().state,
        TeamTaskState::NeedsUser,
        "拒否したまま Validating で止めてはいけない"
    );
    assert!(
        !rt.task(tid).unwrap().validation.running,
        "実行していないのに running のまま"
    );
}

#[test]
fn 承認は保存され復元後に聞き直さない() {
    let (mut rt, sids, tid) = to_assigned();
    report_and_collect(&mut rt, &sids, tid, 12);
    let did = rt
        .decisions()
        .iter()
        .find(|d| d.kind == DecisionKind::ValidationExecution)
        .expect("実行許可")
        .id;
    rt.apply_action(TeamAction::ApproveDecision(did));
    let saved = rt.to_saved();
    let mut r = TeamRuntime::restore(saved, ws());
    r.apply_action(TeamAction::Start);
    // 復元後、担当は外れて Ready へ戻るが、承認そのものは残っている。
    assert!(
        r.to_saved()
            .run
            .approved_validation
            .iter()
            .any(|c| c.contains("cargo test")),
        "承認が保存されていない"
    );
    let _ = (sids, tid);
}

#[test]
fn 走っていた検証は復元後にもう一度走る() {
    // **検証は裏スレッドで走る。** 実行側は「走らせ始めた」時点で成功を返す
    // ので `validate:{task}` は `Completed` になるが、結果が戻る前に落ちれば
    // その実行はプロセスごと消えている。ここで記録を残したままにすると、
    // 復元後に誰も検証を発行せず、タスクは `Validating` で永久に止まる
    // (発行済みで止まるのと同じ事故が、成功済みの側から起きる)。
    let (mut rt, sids, tid) = to_assigned();
    let eff = report_approve_and_collect(&mut rt, &sids, tid, 12);
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Validating);
    // 実行側が「走らせ始めた」と返す (結果はまだ戻っていない)。
    let key = eff
        .iter()
        .find(|e| matches!(e, TeamEffect::RunValidation(_)))
        .expect("検証の発行")
        .key();
    rt.note_effect_done(&key);

    let saved = rt.to_saved();
    let mut r = TeamRuntime::restore(saved, ws());
    r.apply_action(TeamAction::Start);
    let e = r.tick(&obs(20, &[]));
    assert_eq!(
        r.task(tid).unwrap().state,
        TeamTaskState::Validating,
        "検証待ちの成果を捨てている"
    );
    assert!(
        e.iter()
            .any(|x| matches!(x, TeamEffect::RunValidation(v) if v.task == tid)),
        "落ちた検証が復元後に発行されない (永久に Validating): {e:?}"
    );
}

#[test]
fn 実行前にクラッシュしたeffectは復元後に再発行される() {
    // **判別できる Effect で見る。** `StartAgent` は「ACK 済みでもセッションが
    // 無ければ撃ち直す」という別の規則にも守られているので、発行と完了を
    // 取り違えても素通りしてしまう (実際にこの検査は最初その形で空回りした)。
    // 指示 (`instr:`) は成果が残る側なので、記録の意味がそのまま出る。
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    for x in &e {
        let k = x.key();
        if !k.is_empty() {
            rt.note_effect_done(&k);
        }
    }
    let e2 = idle_tick(&mut rt, 11, &sids);
    let sent: Vec<String> = e2
        .iter()
        .map(|x| x.key())
        .filter(|k| k.starts_with("instr:"))
        .collect();
    assert!(!sent.is_empty(), "指示が 1 通も出ていない");

    // **ACK を返さずに**保存 → クラッシュ → 復元。
    let saved = rt.to_saved();
    for k in &sent {
        assert!(
            !saved.run.effects.iter().any(|r| &r.key == k
                && r.state == super::persistence::EffectState::Completed),
            "実行していない指示が完了として保存された: {k}"
        );
    }
    let mut r = TeamRuntime::restore(saved, ws());
    r.apply_action(TeamAction::Start);
    // 復元後は担当が外れて配り直しになるので、同じ指示がもう一度出る。
    let e3 = r.tick(&obs(12, &[]));
    let mut next2 = 1;
    let sids2 = bind_all(&mut r, &e3, &mut next2);
    let e4 = idle_tick(&mut r, 13, &sids2);
    let again: Vec<String> = e4
        .iter()
        .map(|x| x.key())
        .filter(|k| k.starts_with("instr:"))
        .collect();
    assert!(
        !again.is_empty(),
        "実行前にクラッシュした指示が復元後に再発行されない"
    );
    for k in &again {
        assert!(
            !r.effect_completed(k),
            "実行していない指示が完了扱いのままだった: {k}"
        );
    }
}

#[test]
fn 成功ackした指示は復元後に再送されない() {
    // **成果が残る Effect** (届いた指示) は、ACK 後は二度と出さない。
    // `StartAgent` は成果が「生きているセッション」なので別扱い
    // (下の 2 本を参照)。
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    for x in &e {
        let k = x.key();
        if !k.is_empty() {
            rt.note_effect_done(&k);
        }
    }
    let e2 = idle_tick(&mut rt, 11, &sids);
    let sent: Vec<String> = e2
        .iter()
        .map(|x| x.key())
        .filter(|k| k.starts_with("instr:"))
        .collect();
    assert!(!sent.is_empty(), "指示が 1 通も出ていない");
    for k in &sent {
        rt.note_effect_done(k);
    }
    let saved = rt.to_saved();
    let mut r = TeamRuntime::restore(saved, ws());
    r.apply_action(TeamAction::Start);
    let e3 = r.tick(&obs(12, &[]));
    for k in &sent {
        assert!(
            r.effect_completed(k),
            "ACK 済みの指示が復元されていない: {k}"
        );
    }
    assert!(
        !e3.iter().any(|x| x.key().starts_with("instr:") && sent.contains(&x.key())),
        "ACK 済みの指示を再送した: {e3:?}"
    );
}

#[test]
fn 失敗ackしたeffectは再試行される() {
    let mut rt = started(4);
    let eff = rt.tick(&obs(10, &[]));
    let first: Vec<String> = eff
        .iter()
        .map(|e| e.key())
        .filter(|k| k.starts_with("start:"))
        .collect();
    assert!(!first.is_empty());
    for k in &first {
        rt.note_effect_failed(k);
    }
    let eff2 = rt.tick(&obs(11, &[]));
    let again: Vec<String> = eff2
        .iter()
        .map(|e| e.key())
        .filter(|k| k.starts_with("start:"))
        .collect();
    assert_eq!(again, first, "失敗 ACK 後に再試行されない");
}

#[test]
fn start_agent再試行で既存セッションを重複起動しない() {
    let mut rt = started(4);
    let eff = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &eff, &mut next);
    assert!(!sids.is_empty());
    // 結び付いた後は、失敗 ACK を撃っても起動要求は出ない
    for e in &eff {
        let k = e.key();
        if k.starts_with("start:") {
            rt.note_effect_failed(&k);
        }
    }
    let eff2 = idle_tick(&mut rt, 11, &sids);
    assert!(
        !eff2.iter().any(|e| matches!(e, TeamEffect::StartAgent(_))),
        "既にセッションが居るのに起動要求を出した: {eff2:?}"
    );
}

#[test]
fn 検証は同時に二重実行しない() {
    let (mut rt, sids, tid) = to_assigned();
    let e1 = report_approve_and_collect(&mut rt, &sids, tid, 12);
    assert_eq!(
        e1.iter()
            .filter(|e| matches!(e, TeamEffect::RunValidation(_)))
            .count(),
        1
    );
    // 実行中はもう 1 本出さない
    let e2 = idle_tick(&mut rt, 13, &sids);
    assert!(
        !e2.iter().any(|e| matches!(e, TeamEffect::RunValidation(_))),
        "検証を二重に発行した: {e2:?}"
    );
}

#[test]
fn effectの状態が保存復元される() {
    let mut rt = started(4);
    let eff = rt.tick(&obs(10, &[]));
    let keys: Vec<String> = eff.iter().map(|e| e.key()).filter(|k| !k.is_empty()).collect();
    assert!(!keys.is_empty());
    // 1 件だけ ACK して保存
    rt.note_effect_done(&keys[0]);
    let saved = rt.to_saved();
    let r = TeamRuntime::restore(saved, ws());
    assert!(r.effect_completed(&keys[0]), "完了した Effect が復元されない");
    for k in &keys[1..] {
        assert!(
            !r.effect_completed(k),
            "ACK していない Effect が完了扱いで復元された: {k}"
        );
    }
}

#[test]
fn effect履歴の上限処理で未完了を消さない() {
    let mut rt = started(4);
    let eff = rt.tick(&obs(10, &[]));
    let pending: Vec<String> = eff.iter().map(|e| e.key()).filter(|k| !k.is_empty()).collect();
    assert!(!pending.is_empty());
    // 完了済みを大量に積んで刈り取りを起こす
    for i in 0..(EFFECT_KEY_CAP + 100) {
        let k = format!("synthetic:{i}");
        rt.note_effect_dispatched_for_test(&k);
        rt.note_effect_done(&k);
    }
    for k in &pending {
        assert!(
            !rt.effect_completed(k),
            "未完了の Effect が刈り取りで完了扱いになった: {k}"
        );
    }
}

#[test]
fn ack後に結び付けられなかった起動要求は撃ち直される() {
    // 実行側が「起動した」と返してから `bind_session` を呼ぶまでの間に
    // 落ちると、記録は成功のまま・エージェントは居ない、という状態になる。
    // **そこで諦めると、そのエージェントは永久に起動されない。**
    let mut rt = started(4);
    let eff = rt.tick(&obs(10, &[]));
    let keys: Vec<String> = eff
        .iter()
        .map(|e| e.key())
        .filter(|k| k.starts_with("start:"))
        .collect();
    assert!(!keys.is_empty());
    // ACK だけ返して、結び付けは行わない
    for k in &keys {
        rt.note_effect_done(k);
    }
    let eff2 = rt.tick(&obs(11, &[]));
    let again: Vec<String> = eff2
        .iter()
        .map(|e| e.key())
        .filter(|k| k.starts_with("start:"))
        .collect();
    assert_eq!(again, keys, "結び付いていない起動要求が撃ち直されない");
}

// ══════════════════════════════════════════════════════════════════════
//  状態機械を迂回してよい場所を、理由ごと固定する
//
//  `sm::force` は表に無い遷移を通す唯一の抜け道なので、**どこで・なぜ**
//  使ってよいかをテストで留めておく。増えたらここが赤くなる。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn 人の操作だけがneeds_userから戻せる() {
    // 自動処理に `NeedsUser → Ready` は無い (表が拒否する)。
    assert!(super::state_machine::apply(
        TeamTaskState::NeedsUser,
        TeamTaskState::Ready
    )
    .is_err());

    // 人が Retry を押したときだけ戻る。
    let (mut rt, _sids, tid) = to_assigned();
    rt.set_state_for_test(tid, TeamTaskState::NeedsUser);
    rt.apply_action(TeamAction::RetryTask(tid));
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Ready);
}

#[test]
fn 停止を確認できたときだけ実行中から回収できる() {
    // 自動処理に `Running → Ready` は無い。
    assert!(super::state_machine::apply(
        TeamTaskState::Running,
        TeamTaskState::Ready
    )
    .is_err());

    // セッションの消滅を観測したときだけ回収する (free_task)。
    let (mut rt, sids, tid) = to_assigned();
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Running);
    let dead = rt.task(tid).unwrap().assigned_session.unwrap();
    let alive: Vec<SessionId> = sids.iter().copied().filter(|s| *s != dead).collect();
    idle_tick(&mut rt, 12, &alive);
    assert_ne!(rt.task(tid).unwrap().assigned_session, Some(dead));
}

#[test]
fn 効かなかったretryは停止承認を消さない() {
    // Retry が効くのは `NeedsUser` / `Failed` のときだけ。実行中のタスクへ
    // 撃っても状態は変わらないが、**承認待ちの Decision まで一緒に消える**と
    // 「停止待ちの印だけが残り、誰も止めない」タスクができる (画面からは
    // 承認ボタンが消えているので、人からは手が出せない)。
    let (mut rt, sids, tid) = to_assigned();
    rt.apply_action(TeamAction::ReassignTask(tid));
    assert!(
        rt.decisions().iter().any(|d| d.task_id == Some(tid)),
        "停止承認を求めていない"
    );
    rt.apply_action(TeamAction::RetryTask(tid));
    let pending = rt.task(tid).unwrap().reassign_pending;
    let asked = rt.decisions().iter().any(|d| d.task_id == Some(tid));
    assert_eq!(
        pending, asked,
        "停止待ちの印と承認要求が食い違っている (pending={pending} / decision={asked})"
    );
    // 承認できるなら、承認は今も通る。
    if asked {
        let did = rt.decisions().iter().find(|d| d.task_id == Some(tid)).unwrap().id;
        let out = rt.apply_action(TeamAction::ApproveDecision(did));
        assert!(
            out.iter().any(|e| matches!(e, TeamEffect::StopAgent(_))),
            "承認しても停止しない"
        );
    }
    let _ = sids;
}

#[test]
fn 迂回してよい場所は二か所だけ() {
    // **`sm::force` を増やしたらここが赤くなる。** 増やすなら、その場所と
    // 「なぜ確認済みと言えるか」をこのテストにも書くこと。
    let src = include_str!("runtime.rs").replace("\r\n", "\n");
    let n = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .filter(|l| l.contains("sm::force("))
        .count();
    assert_eq!(
        n, 2,
        "状態機械を迂回している箇所が {n} 個ある (人の Retry と、停止確認後の回収だけのはず)"
    );
}
