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
    let plan = StaticPlanner
        .plan(PlanInput {
            spec: SPEC.to_string(),
            source: "SPEC.md".into(),
            agent_count: agents,
            review_required: true,
        })
        .expect("計画できるべき");
    let mut rt = TeamRuntime::from_plan(
        plan,
        ws(),
        RunOptions {
            run_id: "run-test".into(),
            spec_source: "SPEC.md".into(),
            agent_count: agents,
            max_attempts: 3,
            review_required: true,
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
    // **Completed にはならない。** レビュー待ちになる。
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
    });
    assert!(bad.is_err());
    // 制御文字だらけの SPEC でも計画は作れるか、Err になるだけ
    let weird = StaticPlanner.plan(PlanInput {
        spec: "\u{0}\u{1}#\n- \u{7}\n".into(),
        source: "x".into(),
        agent_count: 1,
        review_required: true,
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
fn 冪等キーは復元後も効く() {
    let mut rt = started(4);
    let first = rt.tick(&obs(10, &[]));
    assert!(first.iter().any(|e| matches!(e, TeamEffect::StartAgent(_))));
    let restored_effects = {
        let saved = rt.to_saved();
        let mut r = TeamRuntime::restore(saved, ws());
        r.apply_action(TeamAction::Start);
        r.tick(&obs(11, &[]))
    };
    assert!(
        !restored_effects
            .iter()
            .any(|e| matches!(e, TeamEffect::StartAgent(_))),
        "復元後に同じ起動要求を撃ち直した: {restored_effects:?}"
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
