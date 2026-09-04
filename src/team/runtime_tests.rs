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
            workspace_root: ws(),
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
            agent_presets: Vec::new(),
            max_attempts: 3,
            review_required,
            guardrails: Default::default(),
        },
    );
    rt.apply_action(TeamAction::Start);
    // **実測の入口だけを差し替える。**
    //
    // ここで確かめたいのは調停の判断 (受理・却下・差し戻し) であって、
    // git の読み方ではない。測ること自体は `changeset_tests` が**実 git**
    // で確かめ、Runtime が本当に測って本当に断ることは
    // `実測で担当外を掴んだら完了にしない` が**実 git + 実タスク**で
    // 確かめる。ここを実リポジトリにすると、判断のテスト 200 本ぶんの
    // `git init` を毎回払うことになる。
    //
    // 既定は「測れた・担当範囲は宣言されていない」= StaticPlanner の
    // 計画 (`files` が空) で実際に起きる形。
    test_hooks::set_baseline(Some(super::changeset::FileBaseline {
        complete: true,
        head_commit: "0".repeat(40),
        ..Default::default()
    }));
    test_hooks::set_evidence(Some(rp::FileEvidence::NoScope {
        measured: Vec::new(),
    }));
    rt
}

/// 全セッションを Idle として観測する (crash_tests と共用)。
///
/// **観測の組み立てを 2 か所に書かない。** 書くと、片方だけがセッションを
/// 載せ忘れて「消えた」と解釈される (実際に踏んだ罠)。
pub fn obs_for_test(now: u64, sessions: &[SessionId]) -> Observation {
    let rows: Vec<(SessionId, SessionState, &str)> = sessions
        .iter()
        .map(|s| (*s, SessionState::Idle, ""))
        .collect();
    obs(now, &rows)
}

/// 開始済みの Runtime (crash_tests と共用)。
pub fn started_for_test(agents: usize) -> TeamRuntime {
    started(agents)
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
            rt.bind_session(&s.agent_id, *next, None);
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
    let generation = rt.snapshot_generation();
    tick_text(&mut rt, 12, &sids, sids[0], &ev);
    assert!(
        rt.snapshot_generation() > generation,
        "ReportedSubAgent の追加が snapshot 世代へ反映されない"
    );
    let sub = rt
        .agent(&AgentId::new("backend-test-1"))
        .expect("サブエージェントが登録されるべき");
    assert_eq!(sub.kind, AgentKind::ReportedSubAgent);
    assert!(
        !sub.can_open_terminal(),
        "開けない端末のボタンを出してしまう"
    );
    assert_eq!(sub.parent_id, Some(parent));

    // 同じ画面と同じ構造化ブロックは再取り込みしない。
    let generation = rt.snapshot_generation();
    tick_text(&mut rt, 13, &sids, sids[0], &ev);
    assert_eq!(
        rt.snapshot_generation(),
        generation,
        "同じ ReportedSubAgent 報告で snapshot 世代が進んだ"
    );
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
        workspace_root: ws(),
        roles: Vec::new(),
    });
    assert!(bad.is_err());
    // 制御文字だらけの SPEC でも計画は作れるか、Err になるだけ
    let weird = StaticPlanner.plan(PlanInput {
        spec: "\u{0}\u{1}#\n- \u{7}\n".into(),
        source: "x".into(),
        agent_count: 1,
        review_required: true,
        workspace_root: ws(),
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
                    workspace_root: ws(),
                    roles: Vec::new(),
                })
                .expect("計画できるべき");
            TeamRuntime::from_plan(
                plan,
                ws(),
                RunOptions {
                    agent_count: agents,
                    agent_presets: Vec::new(),
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
            workspace_root: ws(),
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
    let labels: Vec<String> = t.validation_commands.iter().map(|c| c.display()).collect();
    let cmds: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
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
//  検証の実行承認は「その 1 回」にしか効かない
//
//  コマンド文字列だけで承認を使い回すと、承認したときとは**別のコード**が
//  走る。エージェントは build.rs / テスト本体 / Makefile を書き換えられる
//  ので、「同じ `cargo test` だから承認済み」は成り立たない。
// ══════════════════════════════════════════════════════════════════════

/// 保留中の検証実行許可を 1 件返す。
fn pending_exec_decision(rt: &TeamRuntime, tid: TaskId) -> Option<Decision> {
    rt.decisions()
        .iter()
        .find(|d| d.kind == DecisionKind::ValidationExecution && d.task_id == Some(tid))
        .cloned()
}

/// 2 本のタスクをそれぞれ担当へ割り当てた状態。
fn two_assigned() -> (TeamRuntime, Vec<SessionId>, TaskId, TaskId) {
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    let a = assignments(&rt);
    assert!(a.len() >= 2, "実装 2 本が配られる前提が崩れた: {a:?}");
    (rt, sids, a[0].1, a[1].1)
}

#[test]
fn 保存ファイルの中のworkspaceは復元後のcwdを決めない() {
    // 置き場のファイルも**未信頼**として扱う (書き換えられうる)。復元時の
    // workspace は、いま開いているものだけが決める — `run.json` の中の
    // 文字列で検証の実行場所が変わってはいけない。
    let (mut rt, sids, tid) = to_assigned();
    report_approve_and_collect(&mut rt, &sids, tid, 12);
    let mut saved = rt.to_saved();
    saved.run.workspace = if cfg!(windows) {
        "C:\\Windows".to_string()
    } else {
        "/".to_string()
    };
    let trusted = ws();
    let mut r = TeamRuntime::restore(saved, trusted.clone());
    r.apply_action(TeamAction::Start);
    let eff = r.tick(&obs(20, &[]));
    let spec = eff
        .iter()
        .find_map(|e| match e {
            TeamEffect::RunValidation(v) => Some(v.clone()),
            _ => None,
        })
        .expect("復元後に検証が発行される");
    assert_eq!(
        spec.cwd, trusted,
        "保存ファイルの中の workspace で検証を走らせている"
    );
}

#[test]
fn 別のタスクは同じコマンドでも再承認が要る() {
    let (mut rt, sids, ta, tb) = two_assigned();
    // 2 本とも同じ検証コマンドを持つ。
    assert_eq!(
        rt.task(ta).unwrap().validation_commands,
        rt.task(tb).unwrap().validation_commands
    );
    report_and_collect(&mut rt, &sids, ta, 12);
    let d = pending_exec_decision(&rt, ta).expect("A の実行許可");
    rt.apply_action(TeamAction::ApproveDecision(d.id));

    // B が報告しても、A の承認では走らない。
    report_and_collect(&mut rt, &sids, tb, 13);
    let eff = idle_tick(&mut rt, 14, &sids);
    assert!(
        !eff.iter()
            .any(|e| matches!(e, TeamEffect::RunValidation(v) if v.task == tb)),
        "別タスクの承認を使い回した: {eff:?}"
    );
    assert!(
        pending_exec_decision(&rt, tb).is_some(),
        "B について改めて承認を求めていない"
    );
}

#[test]
fn 差し戻したあとの再検証は再承認が要る() {
    let (mut rt, sids, tid) = to_assigned();
    report_approve_and_collect(&mut rt, &sids, tid, 12);
    let gen1 = rt.task(tid).unwrap().validation.generation;
    // 実測が落ちて差し戻される
    note_outcome(&mut rt, tid, 1, ValidationOutcome::Failed);
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Ready);
    // もう一度配られ、実装し直して報告する
    let e = idle_tick(&mut rt, 20, &sids);
    let _ = e;
    report_and_collect(&mut rt, &sids, tid, 21);
    let gen2 = rt.task(tid).unwrap().validation.generation;
    assert!(gen2 > gen1, "検証の世代が進んでいない ({gen1} → {gen2})");
    let eff = idle_tick(&mut rt, 22, &sids);
    assert!(
        !eff.iter()
            .any(|e| matches!(e, TeamEffect::RunValidation(_))),
        "前回の承認で新しいコードを走らせた: {eff:?}"
    );
    assert!(
        pending_exec_decision(&rt, tid).is_some(),
        "作り直したのに承認を求めていない"
    );
}

#[test]
fn レビュー指摘のあとの再検証も再承認が要る() {
    let (mut rt, sids, tid) = to_assigned();
    report_approve_and_collect(&mut rt, &sids, tid, 12);
    validate_ok(&mut rt, tid);
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Reviewing);
    // レビュー担当が配られるまで 1 tick 回す。
    idle_tick(&mut rt, 13, &sids);
    let rev_sid = rt
        .tasks()
        .iter()
        .find(|t| t.review_of == Some(tid))
        .and_then(|t| t.assigned_session)
        .expect("レビュー担当セッション");
    let block = review_block(tid, false);
    tick_text(&mut rt, 14, &sids, rev_sid, &block);
    assert!(
        matches!(
            rt.task(tid).unwrap().state,
            TeamTaskState::Ready | TeamTaskState::Running
        ),
        "REQUEST_CHANGES で差し戻されていない: {:?}",
        rt.task(tid).unwrap().state
    );
    // 直して再報告 → **もう一度承認が要る**
    idle_tick(&mut rt, 15, &sids); // 配り直し
    report_and_collect(&mut rt, &sids, tid, 16);
    assert_eq!(
        rt.task(tid).unwrap().state,
        TeamTaskState::Validating,
        "検証待ちになっていない (検査が空回りする)"
    );
    let eff = idle_tick(&mut rt, 17, &sids);
    assert!(
        !eff.iter()
            .any(|e| matches!(e, TeamEffect::RunValidation(v) if v.task == tid)),
        "指摘を直したコードを、前回の承認で走らせた: {eff:?}"
    );
    assert!(
        pending_exec_decision(&rt, tid).is_some(),
        "直したコードについて承認を求めていない"
    );
}

#[test]
fn 古い承認判断をあとから通しても現在の世代には効かない() {
    // **判断が生き残ったまま世代が進む筋書き。** 承認待ちのあいだに担当の
    // セッションが消えると、タスクは回収されて配り直される。そこで実装し
    // 直した別のコードに対して、**前の回の承認要求**が人の画面に残る。
    // それを押しても、いまの世代を通してはいけない。
    let (mut rt, sids, tid) = to_assigned();
    report_and_collect(&mut rt, &sids, tid, 12);
    let stale = pending_exec_decision(&rt, tid).expect("最初の実行許可");
    let dead = rt.task(tid).unwrap().assigned_session.expect("担当");

    // 担当セッションが消える → 回収されて Ready へ戻る。
    let alive: Vec<SessionId> = sids.iter().copied().filter(|s| *s != dead).collect();
    let mut now = 13;
    while rt.task(tid).unwrap().assigned_session.is_none() && now < 20 {
        idle_tick(&mut rt, now, &alive);
        now += 1;
    }
    // 配り直された先で、直したコードを報告する (= 新しい検証回)。
    idle_tick(&mut rt, now, &alive);
    now += 1;
    report_and_collect(&mut rt, &alive, tid, now);
    now += 1;
    assert_eq!(
        rt.task(tid).unwrap().state,
        TeamTaskState::Validating,
        "検証待ちになっていない (検査が空回りする)"
    );
    let now_gen = rt.task(tid).unwrap().validation.generation;
    assert_ne!(
        stale.validation_generation,
        Some(now_gen),
        "世代が進んでいないので検査にならない"
    );
    // **古い判断がまだ画面に残っている**ことを確かめてから押す。
    assert!(
        rt.decisions().iter().any(|d| d.id == stale.id),
        "検査の前提 (古い判断が残っている) が崩れた"
    );
    rt.apply_action(TeamAction::ApproveDecision(stale.id));
    let eff = idle_tick(&mut rt, now, &alive);
    assert!(
        !eff.iter()
            .any(|e| matches!(e, TeamEffect::RunValidation(_))),
        "古い承認で現在の世代を走らせた: {eff:?}"
    );
    assert!(
        pending_exec_decision(&rt, tid).is_some(),
        "いまの世代について、承認を求めたままになっていない"
    );
}

#[test]
fn 承認は保存され復元しても世代ごとに効く() {
    let (mut rt, sids, tid) = to_assigned();
    report_and_collect(&mut rt, &sids, tid, 12);
    let d = pending_exec_decision(&rt, tid).expect("実行許可");
    rt.apply_action(TeamAction::ApproveDecision(d.id));
    let gen = rt.task(tid).unwrap().validation.generation;

    let saved = rt.to_saved();
    assert!(
        saved
            .run
            .validation_approvals
            .iter()
            .any(|a| a.task_id == tid && a.generation == gen),
        "承認が世代つきで保存されていない: {:?}",
        saved.run.validation_approvals
    );
    let mut r = TeamRuntime::restore(saved, ws());
    r.apply_action(TeamAction::Start);
    // 検証待ちのまま復元されるので、聞き直さずに走る。
    let eff = r.tick(&obs(20, &[]));
    assert!(
        eff.iter()
            .any(|e| matches!(e, TeamEffect::RunValidation(v) if v.task == tid)),
        "復元後に承認済みの検証が走らない: {eff:?}"
    );
    let _ = sids;
}

#[test]
fn 実行しないと決まっているコマンドは承認しても走らない() {
    let (mut rt, sids, tid) = to_assigned();
    // 実行してはいけないコマンドを検証に混ぜる。
    rt.set_validation_commands_for_test(tid, &["git push origin main"]);
    report_and_collect(&mut rt, &sids, tid, 12);
    // 承認できるものは全部承認してしまう (人が押し間違えた筋書き)。
    let ids: Vec<u64> = rt.decisions().iter().map(|d| d.id).collect();
    for id in ids {
        rt.apply_action(TeamAction::ApproveDecision(id));
    }
    for now in 13..18 {
        let eff = idle_tick(&mut rt, now, &sids);
        assert!(
            !eff.iter()
                .any(|e| matches!(e, TeamEffect::RunValidation(_))),
            "禁止コマンドを承認で実行した: {eff:?}"
        );
    }
}

#[test]
fn 書き換える検証は承認前に走らない() {
    // **`black .` は読むだけではない。** 名前が整形ツールでも、旗ひとつで
    // ファイルをその場で書き換える。人の承認なしに AI の計画が
    // workspace を書き換えられてはいけない。
    let (mut rt, sids, tid) = to_assigned();
    rt.set_validation_commands_for_test(tid, &["black ."]);
    let eff = report_and_collect(&mut rt, &sids, tid, 12);
    assert!(
        !eff.iter()
            .any(|e| matches!(e, TeamEffect::RunValidation(_))),
        "承認前に書き換える検証を実行した: {eff:?}"
    );
    let d = pending_exec_decision(&rt, tid).expect("承認を求めていない");
    assert!(
        d.reason.contains("書き換え"),
        "何が起きるのかを伝えていない: {}",
        d.reason
    );
    // 何度回しても出ない。
    for now in 13..17 {
        let eff = idle_tick(&mut rt, now, &sids);
        assert!(!eff
            .iter()
            .any(|e| matches!(e, TeamEffect::RunValidation(_))));
    }
    // 承認したら走る (永久に詰まらせない)。
    rt.apply_action(TeamAction::ApproveDecision(d.id));
    let eff = idle_tick(&mut rt, 17, &sids);
    assert!(
        eff.iter()
            .any(|e| matches!(e, TeamEffect::RunValidation(v) if v.task == tid)),
        "承認しても走らない: {eff:?}"
    );
}

#[test]
fn 読むだけの検証は構造のまま実行器へ渡る() {
    // **判定したものと、実行するものが同じ形であること。** 文字列へ
    // 戻して実行側で割り直すと、引用符の扱い 1 つでずれる。
    let (mut rt, sids, tid) = to_assigned();
    rt.set_validation_commands_for_test(tid, &["shellcheck \"tools/my script.sh\""]);
    // 報告の JSON を通さずに検証待ちへ入れる (この検査の主題は、発行された
    // 要求が構造のままかどうか — 報告文の組み立ては別のテストが見ている)。
    rt.set_state_for_test(tid, TeamTaskState::Validating);
    let eff = idle_tick(&mut rt, 12, &sids);
    let v = eff
        .iter()
        .find_map(|e| match e {
            TeamEffect::RunValidation(v) => Some(v.clone()),
            _ => None,
        })
        .expect("読むだけなので承認なしで発行される");
    assert_eq!(v.commands.len(), 1);
    assert_eq!(v.commands[0].executable, "shellcheck");
    assert_eq!(
        v.commands[0].args,
        vec!["tools/my script.sh".to_string()],
        "引用符を跨いで割れている"
    );
    let _ = sids;
}

#[test]
fn 安全なコマンドは承認を求めない() {
    let (mut rt, sids, tid) = to_assigned();
    rt.set_validation_commands_for_test(tid, &["rustfmt --check src/a.rs"]);
    let eff = report_and_collect(&mut rt, &sids, tid, 12);
    assert!(
        eff.iter()
            .any(|e| matches!(e, TeamEffect::RunValidation(v) if v.task == tid)),
        "リポジトリのコードを実行しないものにまで承認を求めた: {eff:?}"
    );
    assert!(pending_exec_decision(&rt, tid).is_none());
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
        .map(|c| ValidationRun::new(c.display(), code, out))
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

// ══════════════════════════════════════════════════════════════════════
//  保存 → プロセス終了 → 復元 → 再調停
// ══════════════════════════════════════════════════════════════════════

#[test]
fn 保存して復元しても未実行のeffectだけを撃ち直す() {
    // **ディスクを本当に通す。** 「保存したつもり」で復元経路が動いて
    // いなければ、再起動のたびに Run が静かに壊れる。
    use super::persistence;

    let dir = crate::test_util::unique_temp_dir("zaivern-team-restore", "effects");
    let (rt, sids, tid) = to_assigned();
    // 実測の差し替えは「復元後の Runtime」にも要る (別の Runtime なので)。
    let base = super::changeset::FileBaseline {
        complete: true,
        head_commit: "0".repeat(40),
        ..Default::default()
    };

    // 指示が飛んだところまで進める。
    let t = rt.task(tid).unwrap();
    assert_eq!(t.state, TeamTaskState::Running, "配られていない");
    let dispatched: Vec<String> = rt
        .to_saved()
        .run
        .effects
        .iter()
        .map(|e| e.key.clone())
        .collect();
    assert!(!dispatched.is_empty(), "Effect が 1 件も記録されていない");

    persistence::save(&dir, &rt.to_saved()).expect("保存");
    drop(rt); // プロセスが落ちたのと同じ

    // 復元する。
    let saved = match persistence::load(&dir) {
        persistence::LoadOutcome::Loaded(s) => *s,
        other => panic!("復元できない: {other:?}"),
    };
    let mut back = TeamRuntime::restore(saved, PathBuf::from("/zaivern-team-test-workspace"));
    test_hooks::set_baseline(Some(base));
    test_hooks::set_evidence(Some(rp::FileEvidence::NoScope {
        measured: Vec::new(),
    }));

    // **端末は落ちているので、起動はやり直す** (プロセスは戻ってこない)。
    // やり直してはいけないのは「もう届いた指示」のほう。
    let e = back.tick(&obs(20, &[]));
    let mut next2 = 100;
    let sids2 = bind_all(&mut back, &e, &mut next2);
    let again = idle_tick(&mut back, 21, &sids2);

    // 状態は保たれている。
    assert_eq!(back.task(tid).unwrap().state, TeamTaskState::Running);
    assert!(
        back.task(tid).unwrap().assigned_agent.is_some(),
        "担当が消えた"
    );
    // **同じ鍵の指示は二度と出ない。** 出ると、エージェントは同じ仕事を
    // もう一度させられる (しかも本人は 1 回目の続きのつもりでいる)。
    let sent: Vec<String> = again
        .iter()
        .filter_map(|e| match e {
            TeamEffect::SendInstruction { key, .. } => Some(key.clone()),
            _ => None,
        })
        .collect();
    for k in &sent {
        assert!(
            !dispatched.contains(k),
            "再起動後に同じ Effect をもう一度撃った: {k}\n記録: {dispatched:?}"
        );
    }
    let _ = &sids;
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 保存の途中で落ちても再開できる() {
    // **保存の途中で電源が落ちる**筋書き。新旧が混ざったものを読むと、
    // 承認済みの検証が別のタスクへ当たったり Effect が二重に出たりする。
    use super::persistence::{self, fault_inject, SavePhase};

    let dir = crate::test_util::unique_temp_dir("zaivern-team-restore", "crash");
    let (mut rt, sids, tid) = to_assigned();
    persistence::save(&dir, &rt.to_saved()).expect("1 世代目");
    let title_before = rt.task(tid).unwrap().title.clone();

    // 状態を進めてから、保存の途中で落とす。
    rt.apply_action(TeamAction::Pause);
    fault_inject::fail_at(SavePhase::PrevRetired);
    let r = persistence::save(&dir, &rt.to_saved());
    fault_inject::clear();
    assert!(r.is_err(), "落ちなかった");

    // **前の世代がそのまま読める。**
    let saved = match persistence::load(&dir) {
        persistence::LoadOutcome::Loaded(s) => *s,
        other => panic!("復元できない: {other:?}"),
    };
    assert!(!saved.run.paused, "書き切れていない世代を読んだ");
    let mut back = TeamRuntime::restore(saved, PathBuf::from("/zaivern-team-test-workspace"));
    test_hooks::set_baseline(Some(super::changeset::FileBaseline {
        complete: true,
        head_commit: "0".repeat(40),
        ..Default::default()
    }));
    test_hooks::set_evidence(Some(rp::FileEvidence::NoScope {
        measured: Vec::new(),
    }));
    assert_eq!(back.task(tid).unwrap().title, title_before);
    // **再調停が動く** (止まらない)。端末を建て直せば、そのタスクは
    // また配られる。
    let e = back.tick(&obs(20, &[]));
    let mut next2 = 100;
    let sids2 = bind_all(&mut back, &e, &mut next2);
    idle_tick(&mut back, 21, &sids2);
    assert_eq!(
        back.task(tid).unwrap().state,
        TeamTaskState::Running,
        "復元したあと誰にも配られないまま止まった"
    );
    let _ = &sids;
    std::fs::remove_dir_all(&dir).ok();
}

// ══════════════════════════════════════════════════════════════════════
//  変更ファイルは実測する — 自己申告を証跡にしない
// ══════════════════════════════════════════════════════════════════════

/// 実 git のワークスペースを持つ Runtime (**差し替えを外す**)。
///
/// `changeset_tests` が測り方を、ここが**Runtime が本当に測って本当に
/// 断ること**を確かめる。差し替えたままだと、繋いでいなくても緑になる。
fn real_repo_runtime(name: &str) -> Option<(TeamRuntime, Vec<SessionId>, TaskId, PathBuf)> {
    let d = crate::test_util::unique_temp_dir("zaivern-team-rt-git", name);
    std::fs::create_dir_all(&d).ok()?;
    let run = |args: &[&str]| -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&d)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if !run(&["init", "-q"]) {
        std::fs::remove_dir_all(&d).ok();
        return None;
    }
    run(&["config", "user.email", "t@example.invalid"]);
    run(&["config", "user.name", "t"]);
    run(&["config", "commit.gpgsign", "false"]);
    std::fs::create_dir_all(d.join("src/auth")).ok()?;
    std::fs::write(d.join("src/auth/login.rs"), "fn login() {}\n").ok()?;
    std::fs::write(d.join("secret.rs"), "fn secret() {}\n").ok()?;
    if !run(&["add", "-A"]) || !run(&["commit", "-q", "-m", "init"]) {
        std::fs::remove_dir_all(&d).ok();
        return None;
    }

    let plan = StaticPlanner
        .plan(PlanInput {
            spec: SPEC.to_string(),
            source: "SPEC.md".into(),
            agent_count: 2,
            review_required: false,
            workspace_root: d.clone(),
            roles: Vec::new(),
        })
        .expect("計画できるべき");
    let mut rt = TeamRuntime::from_plan(
        plan,
        d.clone(),
        RunOptions {
            run_id: new_run_id(),
            spec_source: "SPEC.md".into(),
            agent_count: 2,
            agent_presets: Vec::new(),
            max_attempts: 3,
            review_required: false,
            guardrails: Default::default(),
        },
    );
    rt.apply_action(TeamAction::Start);
    // **差し替えを外す** — ここは実測そのものを通す。
    test_hooks::clear();
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    let tid = assignments(&rt).first()?.1;
    // 担当範囲を宣言する (StaticPlanner は `files` を空で作る)。
    //
    // **他のタスクにも別の範囲を与える。** 範囲が空のタスクは
    // `lease::overlaps` から見ると「どこでも重なる」ので、1 本だけに
    // 範囲を付けると配り直しが「担当ファイルが重なる」で永久に止まる
    // (実験を組み立てる側の話で、製品の判定は正しい)。
    rt.set_files_for_test(tid, &["src/auth/"]);
    let others: Vec<TaskId> = rt.tasks().iter().map(|t| t.id).filter(|i| *i != tid).collect();
    for (n, id) in others.iter().enumerate() {
        rt.set_files_for_test(*id, &[&format!("other-{n}/")]);
    }
    Some((rt, sids, tid, d))
}

macro_rules! need_git_rt {
    ($name:literal) => {
        match real_repo_runtime($name) {
            Some(v) => v,
            None => {
                eprintln!("[skip] {} — git を使えません", $name);
                return;
            }
        }
    };
}

#[test]
fn 実測で担当外を掴んだら完了にしない() {
    // **配線まで通して確かめる。** 実 git・実タスク・実 Runtime で、
    // エージェントが担当外のファイルを書き換えたら完了報告が通らない。
    let (mut rt, sids, tid, dir) = need_git_rt!("out-of-scope");
    assert!(
        rt.task(tid).unwrap().baseline.as_ref().is_some_and(|b| b.usable()),
        "配る直前の基準点が取れていない"
    );

    // 担当内 1 つと、**担当外 1 つ**を書き換える。
    std::fs::write(dir.join("src/auth/login.rs"), "fn login() { ok(); }\n").unwrap();
    std::fs::write(dir.join("secret.rs"), "fn secret() { stolen(); }\n").unwrap();

    // **自己申告では担当内しか挙げない** (= 省いて隠す)。
    let t = rt.task(tid).unwrap().clone();
    let agent = t.assigned_agent.clone().unwrap().0;
    let sid = t.assigned_session.unwrap();
    let labels: Vec<String> = t.validation_commands.iter().map(|c| c.display()).collect();
    let cmds: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    let block = result_block(tid, &agent, &cmds, &["src/auth/login.rs"]);
    let rows: Vec<(SessionId, SessionState, &str)> = sids
        .iter()
        .map(|s| (*s, SessionState::Idle, if *s == sid { block.as_str() } else { "" }))
        .collect();
    rt.tick(&obs(12, &rows));

    let t = rt.task(tid).unwrap();
    assert_ne!(
        t.state,
        TeamTaskState::Validating,
        "担当外を書き換えたのに完了報告が通った"
    );
    assert_eq!(t.state, TeamTaskState::Running, "却下後は担当のまま");
    // 理由が本人にも人にも伝わる。
    assert!(
        t.context.iter().any(|c| c.contains("secret.rs")),
        "何が担当外だったかを伝えていない: {:?}",
        t.context
    );
    assert!(
        rt.events().any(|e| e.summary.contains("secret.rs")),
        "却下が事象に残っていない"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 実測が担当内なら通り台帳には実測が載る() {
    let (mut rt, sids, tid, dir) = need_git_rt!("in-scope");
    // 担当内を 2 つ変える (1 つは新規)。**申告するのは 1 つだけ。**
    std::fs::write(dir.join("src/auth/login.rs"), "fn login() { ok(); }\n").unwrap();
    std::fs::write(dir.join("src/auth/token.rs"), "fn token() {}\n").unwrap();

    let t = rt.task(tid).unwrap().clone();
    let agent = t.assigned_agent.clone().unwrap().0;
    let sid = t.assigned_session.unwrap();
    let labels: Vec<String> = t.validation_commands.iter().map(|c| c.display()).collect();
    let cmds: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    let block = result_block(tid, &agent, &cmds, &["src/auth/login.rs"]);
    let rows: Vec<(SessionId, SessionState, &str)> = sids
        .iter()
        .map(|s| (*s, SessionState::Idle, if *s == sid { block.as_str() } else { "" }))
        .collect();
    rt.tick(&obs(12, &rows));

    let t = rt.task(tid).unwrap();
    assert_eq!(t.state, TeamTaskState::Validating, "担当内なのに通らない");
    // **台帳へ載るのは実測。** 申告し忘れた `token.rs` も入る。
    assert!(
        t.changed_files.iter().any(|f| f.contains("token.rs")),
        "申告し忘れたファイルが台帳に無い (自己申告をそのまま載せている): {:?}",
        t.changed_files
    );
    assert_eq!(
        t.reported_files,
        vec!["src/auth/login.rs".to_string()],
        "自己申告は自己申告として残す"
    );
    // 食い違いが人に見える。
    assert!(
        t.context.iter().any(|c| c.contains("報告に無いが実際に変更")),
        "食い違いを黙って捨てた: {:?}",
        t.context
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 配り直しても最初の基準点を使う() {
    // **差し戻して再挑戦させるだけで違反が消える**、を作らない。
    //
    // 配り直しのたびに基準点を取り直すと、前の試行で書いた担当外の
    // ファイルが次の基準点へ焼き込まれ、**二度と見えなくなる**。
    // 基準点は「このタスクが最初に触る前」に固定する。
    let (mut rt, sids, tid, dir) = need_git_rt!("baseline-sticky");
    let first = rt.task(tid).unwrap().baseline.clone().expect("基準点");
    let seq1 = rt.task(tid).unwrap().dispatch_seq;

    // 担当内だけを変えて完了報告 → 受理されて Validating。
    std::fs::write(dir.join("src/auth/login.rs"), "fn login() { v1(); }\n").unwrap();
    let t = rt.task(tid).unwrap().clone();
    let agent = t.assigned_agent.clone().unwrap().0;
    let sid = t.assigned_session.unwrap();
    let labels: Vec<String> = t.validation_commands.iter().map(|c| c.display()).collect();
    let cmds: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    let block = result_block(tid, &agent, &cmds, &["src/auth/login.rs"]);
    let rows: Vec<(SessionId, SessionState, &str)> = sids
        .iter()
        .map(|s| (*s, SessionState::Idle, if *s == sid { block.as_str() } else { "" }))
        .collect();
    rt.tick(&obs(12, &rows));
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Validating);

    // 検証が落ちる → 差し戻して配り直す。
    let exec = rt.current_execution(tid);
    let cmds2 = rt.task(tid).unwrap().validation_commands.clone();
    rt.note_validation_for(
        &exec,
        tid,
        cmds2
            .iter()
            .map(|c| ValidationRun::new(c.display(), 1, ValidationOutcome::Failed))
            .collect(),
    );
    for now in 13..25 {
        if rt.task(tid).unwrap().dispatch_seq > seq1 {
            break;
        }
        idle_tick(&mut rt, now, &sids);
    }
    assert!(
        rt.task(tid).unwrap().dispatch_seq > seq1,
        "配り直されていない (この筋書きが成立していない)"
    );

    // **基準点は最初のまま。**
    assert_eq!(
        rt.task(tid).unwrap().baseline.as_ref(),
        Some(&first),
        "配り直しで基準点を取り直した (前の試行の違反が見えなくなる)"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── 所有の証明は Coordinator が持つ ──────────────────────────────────
//
// 「計画に載っている他タスクの `files`」を他人のものとして除くと、
// **まだ 1 度も配られていないタスクの範囲へ書き込んだ違反が消える**。
// 除いてよいのは、Coordinator が「配った」と言える範囲だけ
// ([`coordinator::claimed`])。
//
// **「いま押さえている」(`occupies`) では狭すぎる。** 作業ツリーの変更は
// タスクが終わっても消えないので、隣が書き終えた瞬間にその範囲を
// 誰のものでもなくすと、隣より前に基準点を取っていた担当の実測に
// そのファイルが現れて「担当外」で落ちる。実機の Run (6 体 / 同じ
// ワークスペース) で 4 件出て、25 分走って完了 0 件だった。

/// 隣のタスクへ**本当に**範囲を握らせる (Coordinator を通す)。
fn grant_scope(rt: &mut TeamRuntime, task: TaskId, files: &[&str]) {
    rt.grant_scope_for_test(task, files, 900);
}

/// 隣のタスクに範囲を手放させる (完了)。
fn release_scope(rt: &mut TeamRuntime, task: TaskId) {
    rt.release_scope_for_test(task);
}

/// Coordinator から見て、そのタスクが範囲を押さえているか。
fn holds_scope(rt: &TeamRuntime, task: TaskId) -> bool {
    rt.holds_scope_for_test(task)
}

/// `tid` の担当として完了報告を出し、tick を 1 回回す。
fn report_complete(rt: &mut TeamRuntime, sids: &[SessionId], tid: TaskId, files: &[&str], now: u64) {
    let t = rt.task(tid).expect("タスク").clone();
    let agent = t.assigned_agent.clone().expect("担当").0;
    let sid = t.assigned_session.expect("担当セッション");
    let labels: Vec<String> = t.validation_commands.iter().map(|c| c.display()).collect();
    let cmds: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    let block = result_block(tid, &agent, &cmds, files);
    let rows: Vec<(SessionId, SessionState, &str)> = sids
        .iter()
        .map(|s| (*s, SessionState::Idle, if *s == sid { block.as_str() } else { "" }))
        .collect();
    rt.tick(&obs(now, &rows));
}

#[test]
fn まだ配られていない他タスクの範囲は他人のものにしない() {
    // **Test 1.** 計画に `src/b.rs` を持つタスクが存在するというだけで
    // 誰でもそこを書き換えられる、という状態を作らない。
    let (mut rt, sids, tid, dir) = need_git_rt!("unassigned-neighbour");
    let other = rt
        .tasks()
        .iter()
        .map(|t| t.id)
        .find(|id| *id != tid)
        .expect("2 本目のタスク");
    // **隣はまだ配られていない。** 計画に `secret.rs` を持っているだけで、
    // 所有権を渡した事実は無い (`real_repo_runtime` は最初の tick で配って
    // しまうので、ここで確実に手放させる)。
    release_scope(&mut rt, other);
    rt.set_files_for_test(other, &["secret.rs"]);
    rt.force_state_for_test(other, TeamTaskState::Pending);
    assert!(
        !holds_scope(&rt, other),
        "前提: 隣はまだ何も押さえていない"
    );

    std::fs::write(dir.join("src/auth/login.rs"), "fn login() { ok(); }\n").unwrap();
    std::fs::write(dir.join("secret.rs"), "fn secret() { stolen(); }\n").unwrap();
    report_complete(&mut rt, &sids, tid, &["src/auth/login.rs"], 12);

    let t = rt.task(tid).unwrap();
    assert_ne!(
        t.state,
        TeamTaskState::Validating,
        "他タスクの「予定」範囲へ書いた違反が見逃された"
    );
    assert!(
        t.context.iter().any(|c| c.contains("secret.rs")),
        "何が担当外だったかを伝えていない: {:?}",
        t.context
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 本当に並列実行中の隣の変更は誤検知しない() {
    // **Test 2.** 隣が正当に押さえている範囲の変更を、こちらの違反として
    // 数えない。数えると、複数エージェントの Run がまともに終わらない。
    let (mut rt, sids, tid, dir) = need_git_rt!("parallel-holder");
    let other = rt
        .tasks()
        .iter()
        .map(|t| t.id)
        .find(|id| *id != tid)
        .expect("2 本目のタスク");
    grant_scope(&mut rt, other, &["secret.rs"]);
    assert!(holds_scope(&rt, other), "前提: 隣が押さえている");

    std::fs::write(dir.join("src/auth/login.rs"), "fn login() { ok(); }\n").unwrap();
    std::fs::write(dir.join("secret.rs"), "fn secret() { theirs(); }\n").unwrap();
    report_complete(&mut rt, &sids, tid, &["src/auth/login.rs"], 12);

    let t = rt.task(tid).unwrap();
    assert_eq!(
        t.state,
        TeamTaskState::Validating,
        "並列実行中の隣の変更を自分の違反として咎めた"
    );
    assert!(
        !t.changed_files.iter().any(|f| f.contains("secret.rs")),
        "隣の変更を自分の成果として数えた: {:?}",
        t.changed_files
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 隣が書き終えた範囲の変更を自分の違反にしない() {
    // **Test 3.** 隣が完了して手を離しても、隣が書いたファイルは作業ツリーに
    // 残り続ける。こちらの基準点が隣より前なら、その変更は**必ず**こちらの
    // 実測に現れる。ここを「誰も押さえていない = 自分ではないと言えない」と
    // 倒すと、**後から報告した担当が全員落ちる**。
    //
    // 実機の Run (6 体・同じワークスペース) で、`#3` が `docs/plan.md` を
    // 書き終えた直後に `#4` と `#7` が同じ理由で却下され、25 分走って完了は
    // 0 件だった。誤検知は「人が見れば分かる」では済まず、Run そのものを
    // 止める。
    //
    // **見逃しは増えていない。** 隣が押さえている間も Test 2 のとおり
    // 遮蔽されているので、延びたのは遮蔽の期間だけ。一度も配られていない
    // 範囲は Test 1 のとおり今までどおり担当外へ倒れる。
    let (mut rt, sids, tid, dir) = need_git_rt!("released-neighbour");
    let other = rt
        .tasks()
        .iter()
        .map(|t| t.id)
        .find(|id| *id != tid)
        .expect("2 本目のタスク");
    grant_scope(&mut rt, other, &["secret.rs"]);
    assert!(holds_scope(&rt, other), "前提: いったんは押さえている");
    // 隣が完了して手放す (Coordinator へも伝える — ここが真実)。
    release_scope(&mut rt, other);
    assert!(!holds_scope(&rt, other), "前提: もう押さえていない");

    std::fs::write(dir.join("src/auth/login.rs"), "fn login() { ok(); }\n").unwrap();
    std::fs::write(dir.join("secret.rs"), "fn secret() { later(); }\n").unwrap();
    report_complete(&mut rt, &sids, tid, &["src/auth/login.rs"], 13);

    let t = rt.task(tid).unwrap();
    assert_eq!(
        t.state,
        TeamTaskState::Validating,
        "隣が書き終えた範囲の変更を自分の違反として咎めた"
    );
    assert!(
        !t.changed_files.iter().any(|f| f.contains("secret.rs")),
        "隣の変更を自分の成果として数えた: {:?}",
        t.changed_files
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 自己申告から省いても実測で担当外が出る() {
    // **Test 4.** 申告を書き換えるだけでは通らない。
    let (mut rt, sids, tid, dir) = need_git_rt!("hidden-report");
    let other = rt
        .tasks()
        .iter()
        .map(|t| t.id)
        .find(|id| *id != tid)
        .expect("2 本目のタスク");
    release_scope(&mut rt, other);
    rt.set_files_for_test(other, &["secret.rs"]);
    rt.force_state_for_test(other, TeamTaskState::Pending);

    std::fs::write(dir.join("src/auth/login.rs"), "fn login() { ok(); }\n").unwrap();
    std::fs::write(dir.join("secret.rs"), "fn secret() { stolen(); }\n").unwrap();
    // **`secret.rs` を意図的に省いた**申告 (何なら空でもよい)。
    for (n, reported) in [vec!["src/auth/login.rs"], vec![]].into_iter().enumerate() {
        report_complete(&mut rt, &sids, tid, &reported, 12 + n as u64);
        assert_ne!(
            rt.task(tid).unwrap().state,
            TeamTaskState::Validating,
            "申告を {reported:?} にしただけで通った"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn 実測できないなら完了にせず人へ渡す() {
    // git 管理外のワークスペース。**保証を偽らない。**
    let (mut rt, sids, tid) = to_assigned();
    test_hooks::set_evidence(Some(rp::FileEvidence::Unavailable(
        "ワークスペースが Git 管理下ではありません".into(),
    )));
    report_and_collect(&mut rt, &sids, tid, 12);
    let t = rt.task(tid).unwrap();
    assert_ne!(
        t.state,
        TeamTaskState::Validating,
        "実測できないのに完了報告を通した"
    );
    assert!(
        t.context.iter().any(|c| c.contains("実測")),
        "測れなかったことが伝わっていない: {:?}",
        t.context
    );
}

#[test]
fn 失敗した検証の出力が次の担当へ渡る() {
    // **「`cargo test` が落ちた」だけでは直せない。** どのテストが・
    // なぜ落ちたかは道具が stdout / stderr に書いている。実行器が拾った
    // ものを、次に配るときの指示文 (`context`) まで運ぶ。
    let (mut rt, sids, tid) = to_assigned();
    report_approve_and_collect(&mut rt, &sids, tid, 12);
    let exec = rt.current_execution(tid);
    let cmds = rt.task(tid).unwrap().validation_commands.clone();
    let runs: Vec<ValidationRun> = cmds
        .iter()
        .map(|c| {
            ValidationRun::new(c.display(), 1, ValidationOutcome::Failed).with_output(
                ValidationOutput {
                    stdout: "test auth::login ... FAILED\nfailures:\n    auth::login\n".into(),
                    stderr: "error[E0308]: mismatched types\n  --> src/auth/login.rs:42:9\n".into(),
                    ..Default::default()
                },
            )
        })
        .collect();
    rt.note_validation_for(&exec, tid, runs);

    let t = rt.task(tid).unwrap();
    let ctx = t.context.join("\n");
    assert!(
        ctx.contains("auth::login"),
        "落ちたテスト名が次の担当へ渡っていない: {:?}",
        t.context
    );
    assert!(
        ctx.contains("E0308") && ctx.contains("src/auth/login.rs:42"),
        "stderr のコンパイルエラーが渡っていない: {:?}",
        t.context
    );
    // **どちらに出たかも分かる。** 道具によって出し先が違う。
    assert!(ctx.contains("stdout") && ctx.contains("stderr"), "{ctx}");
    // **実際に配られる指示文へ載ることまで見る** (`context` を積んだだけで
    // 満足しない — 積んだのに渡らない、が今回のような不具合の形)。
    let eff = idle_tick(&mut rt, 30, &sids);
    let text = eff
        .iter()
        .find_map(|e| match e {
            TeamEffect::SendInstruction { task, text, .. } if *task == tid => Some(text.clone()),
            _ => None,
        })
        .expect("差し戻したタスクが配り直されない");
    assert!(
        text.contains("E0308") && text.contains("auth::login"),
        "指示文へ載っていない (context は積んだのに渡っていない):\n{text}"
    );
    // **1 本ぶんが指示文を埋め尽くさない。**
    assert!(
        text.len() < 40_000,
        "指示文が診断で肥大した: {} バイト",
        text.len()
    );
}

#[test]
fn 診断は再起動しても読める() {
    // 保存 → 読み直しで診断が消えると、再開した人は理由を見られない。
    let (mut rt, sids, tid) = to_assigned();
    report_approve_and_collect(&mut rt, &sids, tid, 12);
    let exec = rt.current_execution(tid);
    let cmds = rt.task(tid).unwrap().validation_commands.clone();
    rt.note_validation_for(
        &exec,
        tid,
        cmds.iter()
            .map(|c| {
                ValidationRun::new(c.display(), 1, ValidationOutcome::Failed).with_output(
                    ValidationOutput {
                        stderr: "error[E0432]: unresolved import".into(),
                        stdout_truncated: true,
                        ..Default::default()
                    },
                )
            })
            .collect(),
    );
    let saved = rt.to_saved();
    let json = serde_json::to_string(&saved.tasks).unwrap();
    let back: Vec<TeamTask> = serde_json::from_str(&json).unwrap();
    let t = back.iter().find(|t| t.id == tid).expect("タスク");
    let out = t
        .validation
        .runs
        .iter()
        .find_map(|r| r.output.as_ref())
        .expect("診断が保存されていない");
    assert!(out.stderr.contains("E0432"));
    assert!(out.stdout_truncated, "切り詰めた印が消えた");
    // 画面 (Inspector) からも読める。
    let vm = super::view_model::snapshot(&rt, now_secs());
    let tv = vm.tasks.iter().find(|x| x.id == tid).expect("タスク");
    assert!(
        tv.validation_diagnostics.iter().any(|d| d.contains("E0432")),
        "Inspector から読めない: {:?}",
        tv.validation_diagnostics
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
        cmds.iter().map(|c| ValidationRun::passed(c.display())).collect(),
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
        cmds.iter().map(|c| ValidationRun::passed(c.display())).collect(),
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
    // **人が Retry を押せば動き出せる。** 前任の保持が解けていないと
    // `PreviousHolderNotStopped` で二度と配れず、`Ready` のまま固まる。
    rt.apply_action(TeamAction::RetryTask(tid));
    let mut now = 14;
    while rt.task(tid).unwrap().assigned_session.is_none() && now < 20 {
        idle_tick(&mut rt, now, &sids);
        now += 1;
    }
    assert!(
        rt.task(tid).unwrap().assigned_session.is_some(),
        "拒否したタスクが Retry でも動かない: {:?}",
        rt.task(tid).unwrap().state
    );
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
fn 検証の世代を進める場所は一つだけ() {
    // **承認の範囲は世代で決まる。** 進める場所が 2 つあると、片方だけを
    // 通った検証が「前の回の承認」で走ってしまう。増やすなら、なぜそこでも
    // 進めてよいのかをこのテストにも書くこと。
    let src = include_str!("runtime.rs").replace("\r\n", "\n");
    let n = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .filter(|l| l.contains("validation.generation = "))
        .count();
    assert_eq!(n, 1, "検証の世代を進めている場所が {n} 個ある");
    // その 1 か所は検証回の始まり。
    let f = src
        .find("fn begin_validation_round")
        .expect("検証回の始まりが無い");
    let end = src[f..]
        .find("\n    }\n")
        .map(|i| f + i)
        .unwrap_or(src.len());
    assert!(
        src[f..end].contains("validation.generation = "),
        "世代を進めているのが `begin_validation_round` の外にある"
    );
}

// ══════════════════════════════════════════════════════════════════════
//  実行コンテキストは、生成地点から実行地点まで運ぶ
// ══════════════════════════════════════════════════════════════════════

/// `tid` が配り直されて、指示が実際に飛ぶまで回す。
///
/// **状態が変わっただけでは足りない。** 前任の保持が解けていないと
/// `PreviousHolderNotStopped` で永久に配れず、冪等キーが完了のままだと
/// 指示が届かない — どちらも「Retry を押したのに何も起きない」になる。
fn runs_again(rt: &mut TeamRuntime, sids: &[SessionId], tid: TaskId, from: u64) -> bool {
    let mut now = from;
    let mut sent = false;
    while now < from + 10 && !sent {
        let eff = idle_tick(rt, now, sids);
        sent = eff
            .iter()
            .any(|e| matches!(e, TeamEffect::SendInstruction { task, .. } if *task == tid));
        now += 1;
    }
    sent && rt.task(tid).is_some_and(|t| t.assigned_session.is_some())
}

#[test]
fn 人が戻せる状態はどれも配り直せる() {
    // **`RetryTask` は「その手前で担当を解放済み」を前提にしている。**
    // 解放し忘れた経路を足すと、押しても `Ready` のまま動かなくなる
    // (状態だけ見るテストでは素通りする)。3 つの入口を全部通す。
    let report = |rt: &mut TeamRuntime, sids: &[SessionId], tid: TaskId, status: &str, now: u64| {
        let agent = rt.task(tid).unwrap().assigned_agent.clone().unwrap().0;
        let sid = rt.task(tid).unwrap().assigned_session.unwrap();
        let block = format!(
            "{open}\n{{\"task_id\":{tid},\"agent_id\":\"{agent}\",\"status\":\"{status}\",\
             \"summary\":\"だめでした\",\"changed_files\":[],\"validation\":[],\
             \"blockers\":[\"詰まりました\"]}}\n{close}",
            open = rp::RESULT_OPEN,
            close = rp::RESULT_CLOSE
        );
        tick_text(rt, now, sids, sid, &block);
    };

    // 1) エージェントが「進められない」と報告した
    let (mut rt, sids, tid) = to_assigned();
    report(&mut rt, &sids, tid, "blocked", 12);
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Blocked);
    rt.apply_action(TeamAction::RetryTask(tid));
    assert!(
        runs_again(&mut rt, &sids, tid, 13),
        "blocked から戻せない: state={:?} session={:?} co={:?}",
        rt.task(tid).unwrap().state,
        rt.task(tid).unwrap().assigned_session,
        rt.task(tid).unwrap().coordinator_task
    );

    // 2) エージェントが「失敗した」と報告した
    let (mut rt, sids, tid) = to_assigned();
    report(&mut rt, &sids, tid, "failed", 12);
    rt.apply_action(TeamAction::RetryTask(tid));
    assert!(runs_again(&mut rt, &sids, tid, 13), "failed から戻せない");

    // 3) 実測が落ちて上限まで行った (NeedsUser)
    let (mut rt, sids, tid) = to_assigned();
    report_approve_and_collect(&mut rt, &sids, tid, 12);
    note_outcome(&mut rt, tid, 1, ValidationOutcome::Failed);
    rt.set_state_for_test(tid, TeamTaskState::NeedsUser);
    rt.apply_action(TeamAction::RetryTask(tid));
    assert!(runs_again(&mut rt, &sids, tid, 20), "needs_user から戻せない");
}

#[test]
fn 進められないという報告は行き止まりにしない() {
    // `Blocked` から自動で出る経路は無い (依存が解けるのは `Pending` だけ)。
    // 判断を出さないと、そのタスクは永久に止まったまま誰も気付かない。
    let (mut rt, sids, tid) = to_assigned();
    let sid = rt.task(tid).unwrap().assigned_session.expect("担当");
    let agent = rt.task(tid).unwrap().assigned_agent.clone().unwrap().0;
    let block = format!(
        "{open}\n{{\"task_id\":{tid},\"agent_id\":\"{agent}\",\"status\":\"blocked\",\
         \"summary\":\"仕様が矛盾しています\",\"changed_files\":[],\"validation\":[],\
         \"blockers\":[\"API の仕様が 2 つある\"]}}\n{close}",
        open = rp::RESULT_OPEN,
        close = rp::RESULT_CLOSE
    );
    tick_text(&mut rt, 12, &sids, sid, &block);
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Blocked);
    assert!(
        rt.decisions().iter().any(|d| d.task_id == Some(tid)),
        "止まったことを人へ上げていない"
    );
    // **人が Retry で戻せる。** 戻せないと画面のボタンが嘘になる。
    rt.apply_action(TeamAction::RetryTask(tid));
    assert_ne!(
        rt.task(tid).unwrap().state,
        TeamTaskState::Blocked,
        "Retry が効かない (画面のボタンが何もしない)"
    );
}

#[test]
fn 能力が足りないときは黙って断り続けない() {
    // 能力は計画で決まるので、次の tick でも同じ結果になる。黙ると
    // 「なぜか Ready のまま動かない」タスクができる。
    let mut rt = started(4);
    let tid = rt.tasks()[0].id;
    rt.set_required_caps_for_test(tid, &["quantum-computing"]);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    idle_tick(&mut rt, 11, &sids);
    idle_tick(&mut rt, 12, &sids);
    assert!(
        !assignments(&rt).iter().any(|(_, t, _)| *t == tid),
        "能力が足りないのに配った"
    );
    assert!(
        rt.decisions()
            .iter()
            .any(|d| d.kind == DecisionKind::NoCandidate && d.task_id == Some(tid)),
        "黙って断り続けている: {:?}",
        rt.decisions().iter().map(|d| (d.kind, d.task_id)).collect::<Vec<_>>()
    );
}

#[test]
fn 消えたエージェントは担当を名乗らない() {
    // **画面と実体をずらさない。** セッションが消えたのにカードが担当を
    // 名乗り続けると、Inspector が効かない Retry / Reassign を出す。
    let (mut rt, sids, tid) = to_assigned();
    let dead = rt.task(tid).unwrap().assigned_session.expect("担当");
    let agent = rt.task(tid).unwrap().assigned_agent.clone().unwrap();
    // 観測は割り当ての**次の** tick で担当を写す。
    idle_tick(&mut rt, 12, &sids);
    assert_eq!(
        rt.agents()
            .iter()
            .find(|a| a.id == agent)
            .and_then(|a| a.current_task),
        Some(tid)
    );
    let alive: Vec<SessionId> = sids.iter().copied().filter(|s| *s != dead).collect();
    idle_tick(&mut rt, 13, &alive);
    let a = rt
        .agents()
        .iter()
        .find(|a| a.id == agent)
        .expect("エージェント");
    assert_eq!(a.session_id, None);
    assert_eq!(
        a.current_task, None,
        "消えたエージェントが担当を名乗ったままになっている"
    );
}

#[test]
fn 方針で止められた指示は撃ち直さず人へ上げる() {
    // **同じ理由で止まるものを毎 tick 送り直さない。** 送り直すぶんだけ
    // 同じ説明が出続けて、前へは 1 ミリも進まない (「エラーに回復経路が
    // 無い」の典型)。人が手当てできる形 (判断) にして止める。
    let (mut rt, sids, tid) = to_assigned();
    let sid = rt.task(tid).unwrap().assigned_session.expect("担当");
    // 実行側は**送れなかった鍵を完了にしない** (完了にすると、手当てして
    // Retry したあとも同じ鍵が抑止されて指示が二度と届かない)。
    let key = format!(
        "instr:{tid}:{}:{}",
        rt.task(tid).unwrap().assigned_agent.clone().unwrap().0,
        rt.task(tid).unwrap().attempts
    );
    rt.note_effect_failed(&key);
    rt.note_instruction_blocked(tid, "コスト上限に達しました");
    let t = rt.task(tid).unwrap();
    assert_eq!(t.state, TeamTaskState::NeedsUser, "人へ上げていない");
    assert!(
        t.context.iter().any(|c| c.contains("コスト上限")),
        "理由が残っていない: {:?}",
        t.context
    );
    assert!(
        rt.decisions()
            .iter()
            .any(|d| d.kind == DecisionKind::CostLimit && d.task_id == Some(tid)),
        "判断として出していない"
    );
    // 撃ち直さない。
    let eff = idle_tick(&mut rt, 20, &sids);
    assert!(
        !eff.iter()
            .any(|e| matches!(e, TeamEffect::SendInstruction { session, .. } if *session == sid)),
        "止められた指示を送り直した: {eff:?}"
    );
    // **手当てしたら Retry で本当に動き出せる。** 状態が変わるだけでは
    // 足りない — 前任の保持が解けていないと `PreviousHolderNotStopped` で
    // 永久に配れず、鍵が完了のままだと指示が届かない。
    rt.apply_action(TeamAction::RetryTask(tid));
    assert_ne!(rt.task(tid).unwrap().state, TeamTaskState::NeedsUser);
    let mut now = 21;
    let mut sent = false;
    while now < 30 && !sent {
        let eff = idle_tick(&mut rt, now, &sids);
        sent = eff
            .iter()
            .any(|e| matches!(e, TeamEffect::SendInstruction { task, .. } if *task == tid));
        now += 1;
    }
    assert!(
        rt.task(tid).unwrap().assigned_session.is_some(),
        "Retry のあと配り直されない: {:?}",
        rt.task(tid).unwrap().state
    );
    assert!(sent, "配り直したのに指示が出ない (冪等キーが抑止している)");
}

#[test]
fn 断られた遷移は黙殺せず記録に残す() {
    // **fail-closed で「そのまま」にするのは正しい。** ただし黙って
    // 無かったことにすると「押したのに何も起きない」を誰も追えない。
    let (mut rt, sids, tid) = to_assigned();
    let before = rt.rejected_transitions();
    // 完了したタスクへ、あとから報告が流れてくる筋書き。
    rt.set_state_for_test(tid, TeamTaskState::Completed);
    report_and_collect(&mut rt, &sids, tid, 12);
    assert_eq!(
        rt.task(tid).unwrap().state,
        TeamTaskState::Completed,
        "完了から動いてしまった"
    );
    assert!(
        rt.rejected_transitions() > before,
        "断ったことを記録に残していない"
    );
    assert!(
        rt.events()
            .any(|e| e.kind == TeamEventKind::TransitionRejected),
        "事象として見えない"
    );
}

#[test]
fn 復元後の状態を表で固定する() {
    // **11 状態すべてについて、復元後にどうなるかを表で決める。**
    // ここが曖昧だと「担当が居ないのに Running のまま待ち続ける」
    // 「出来上がった成果を捨ててもう一度実装させる」が静かに起きる。
    use TeamTaskState::*;
    // (保存時の状態, 復元後の状態, 担当を外すか)
    let table: [(TeamTaskState, TeamTaskState, bool); 11] = [
        // 依存待ち・配布待ちはそのまま。
        (Pending, Pending, false),
        (Ready, Ready, false),
        // **担当が居ないと進まない状態**は空けて Ready へ。
        (Assigned, Ready, true),
        (Running, Ready, true),
        // 検証は Zaivern 自身が走らせるので、成果を捨てずに再開できる。
        (Validating, Validating, true),
        // レビュー待ちの本体はそのまま (レビュータスク側が配り直される)。
        (Reviewing, Reviewing, true),
        // 人・依存を待っている状態は動かさない。
        (Blocked, Blocked, true),
        (RevisionRequired, RevisionRequired, true),
        (Failed, Failed, true),
        (NeedsUser, NeedsUser, true),
        (Completed, Completed, true),
    ];
    for (saved_state, want, _) in table {
        let mut rt = started(4);
        let e = rt.tick(&obs(10, &[]));
        let mut next = 1;
        let sids = bind_all(&mut rt, &e, &mut next);
        idle_tick(&mut rt, 11, &sids);
        let tid = assignments(&rt)[0].1;
        rt.set_state_for_test(tid, saved_state);
        let r = TeamRuntime::restore(rt.to_saved(), ws());
        let t = r.task(tid).expect("タスク");
        assert_eq!(
            t.state, want,
            "{saved_state:?} を復元したら {:?} になった (期待 {want:?})",
            t.state
        );
        // **セッションはどれも生き残っていない。** 結び付きは必ず外れる。
        assert_eq!(
            t.assigned_session, None,
            "{saved_state:?}: 死んだセッションを引き継いだ"
        );
        assert_eq!(t.coordinator_task, None, "{saved_state:?}: 調停層の紐が残った");
        assert!(
            !t.validation.running,
            "{saved_state:?}: 走っていない検証を実行中のままにした"
        );
        for a in r.agents() {
            assert_eq!(a.session_id, None, "{saved_state:?}: エージェントの結び付きが残った");
        }
    }
}

#[test]
fn 復元しても死んだセッションへは指示を出さない() {
    let (rt, sids, tid) = to_assigned();
    let dead = rt.task(tid).unwrap().assigned_session.expect("担当");
    let mut r = TeamRuntime::restore(rt.to_saved(), ws());
    r.apply_action(TeamAction::Start);
    // **観測に死んだセッションは出てこない。** それでも指示は出ない。
    let eff = r.tick(&obs(20, &[]));
    for x in &eff {
        if let TeamEffect::SendInstruction { session, .. } = x {
            assert_ne!(*session, dead, "復元後に死んだセッションへ指示を出した");
        }
    }
    let _ = sids;
}

#[test]
fn 起動要求はruntimeのworkspaceを運ぶ() {
    // **不変条件**: `TeamRuntime.workspace == AgentLaunchSpec.workspace_root`。
    // ここが崩れると、実行側は「いまの画面のフォルダ」を見るしかなくなる。
    let mut rt = started(4);
    let eff = rt.tick(&obs(10, &[]));
    let specs: Vec<_> = eff
        .iter()
        .filter_map(|e| match e {
            TeamEffect::StartAgent(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(!specs.is_empty(), "起動要求が出ていない");
    for s in &specs {
        assert_eq!(s.workspace_root, rt.workspace(), "Run の workspace と違う");
        // 飾りのフィールドを残さない。
        assert!(!s.name.trim().is_empty());
        assert!(!s.agent_id.0.trim().is_empty());
        assert!(!s.team_id.0.trim().is_empty());
    }
    // 持ち主も同じ workspace を指す。
    assert_eq!(rt.owner().workspace, rt.workspace());
    assert_eq!(rt.owner().run_id, rt.run().run_id);
}

#[test]
fn 検証の実行先もruntimeのworkspaceになる() {
    let (mut rt, sids, tid) = to_assigned();
    let eff = report_approve_and_collect(&mut rt, &sids, tid, 12);
    let v = eff
        .iter()
        .find_map(|e| match e {
            TeamEffect::RunValidation(v) => Some(v.clone()),
            _ => None,
        })
        .expect("検証の発行");
    assert_eq!(v.cwd, rt.workspace(), "検証を別の場所で走らせている");
}

#[test]
fn 完了したタスクは自動では戻らない() {
    // 状態機械の終端。**自動処理からは 1 経路も出られない。**
    for to in [
        TeamTaskState::Ready,
        TeamTaskState::Assigned,
        TeamTaskState::Running,
        TeamTaskState::Validating,
        TeamTaskState::Reviewing,
        TeamTaskState::Failed,
        TeamTaskState::NeedsUser,
        TeamTaskState::Blocked,
    ] {
        assert!(
            super::state_machine::apply(TeamTaskState::Completed, to).is_err(),
            "Completed から {to:?} へ自動で戻れてしまう"
        );
    }
    // 実物でも: 完了したタスクへ報告が来ても動かない。
    let (mut rt, sids, tid) = to_assigned();
    report_approve_and_collect(&mut rt, &sids, tid, 12);
    validate_ok(&mut rt, tid);
    let rev = rt
        .tasks()
        .iter()
        .find(|t| t.review_of == Some(tid))
        .map(|t| t.id)
        .expect("レビュータスク");
    idle_tick(&mut rt, 13, &sids);
    let rev_sid = rt
        .task(rev)
        .and_then(|t| t.assigned_session)
        .unwrap_or(sids[0]);
    tick_text(&mut rt, 14, &sids, rev_sid, &review_block(tid, true));
    assert_eq!(rt.task(tid).unwrap().state, TeamTaskState::Completed);
    // もう一度同じ報告が流れてきても完了のまま。
    tick_text(&mut rt, 15, &sids, rev_sid, &review_block(tid, false));
    assert_eq!(
        rt.task(tid).unwrap().state,
        TeamTaskState::Completed,
        "完了したタスクが差し戻された"
    );
}

#[test]
fn レビューは実装したのと別のセッションが持つ() {
    let (mut rt, sids, tid) = to_assigned();
    let impl_sid = rt.task(tid).unwrap().assigned_session.expect("実装担当");
    report_approve_and_collect(&mut rt, &sids, tid, 12);
    validate_ok(&mut rt, tid);
    idle_tick(&mut rt, 13, &sids);
    let rev = rt
        .tasks()
        .iter()
        .find(|t| t.review_of == Some(tid))
        .expect("レビュータスク")
        .clone();
    if let Some(rev_sid) = rev.assigned_session {
        assert_ne!(
            rev_sid, impl_sid,
            "実装したセッションが自分のコードをレビューしている"
        );
    }
}

#[test]
fn 調停層が断ったら指示も出さない() {
    // **`coordinator` を迂回しない。** 断られたタスクは割り当てられず、
    // 割り当てられていない相手へ指示を送らない。
    let mut rt = started(4);
    let e = rt.tick(&obs(10, &[]));
    let mut next = 1;
    let sids = bind_all(&mut rt, &e, &mut next);
    let eff = idle_tick(&mut rt, 11, &sids);
    let assigned: Vec<TaskId> = assignments(&rt).iter().map(|(_, t, _)| *t).collect();
    for x in &eff {
        if let TeamEffect::SendInstruction { session, .. } = x {
            let owner = rt
                .tasks()
                .iter()
                .find(|t| t.assigned_session == Some(*session))
                .map(|t| t.id);
            assert!(
                owner.is_some_and(|t| assigned.contains(&t)),
                "割り当てられていない相手へ指示を出した: session={session}"
            );
        }
    }
    // 依存が残っているタスクは配られず、指示も出ない。
    let integ = rt.tasks().last().expect("統合タスク").id;
    assert!(!assigned.contains(&integ));
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

// ── 人が出した指示 ───────────────────────────────────────────────────

/// 出た `SendManualInstruction` を読みやすい形で取り出す。
fn manual_sends(effects: &[TeamEffect]) -> Vec<(String, SessionId, String, String)> {
    effects
        .iter()
        .filter_map(|e| match e {
            TeamEffect::SendManualInstruction {
                agent,
                session,
                text,
                key,
            } => Some((agent.0.clone(), *session, text.clone(), key.clone())),
            _ => None,
        })
        .collect()
}

/// 担当が付いた状態を作る。返すのは (runtime, その担当のセッション, タスク, 担当名)。
///
/// **既存の [`to_assigned`] を土台にする** (組み立てを 2 か所に書かない)。
fn to_instructable() -> (TeamRuntime, SessionId, TaskId, String) {
    let (rt, _, _) = to_assigned();
    let (sid, tid, agent) = assignments(&rt)[0].clone();
    (rt, sid, tid, agent)
}

/// **人の指示は、選んだ相手の端末へその場で出る。**
///
/// `AddContext` は「次に配るときの文脈」を足すだけで、いま動いている端末へは
/// 1 バイトも届かない。途中で口を出せることを、実際に Effect が出ることで見る。
#[test]
fn 人の指示は選んだエージェントの端末へ出る() {
    let (mut rt, sid, _tid, agent) = to_instructable();
    let out = rt.apply_action(TeamAction::InstructAgent {
        agent: AgentId(agent.clone()),
        text: "  テストを先に書いて  ".into(),
    });
    let sent = manual_sends(&out);
    assert_eq!(sent.len(), 1, "1 通だけ出るべき: {out:?}");
    assert_eq!(sent[0].0, agent, "宛先が違う");
    assert_eq!(sent[0].1, sid, "端末が違う");
    assert_eq!(sent[0].2, "テストを先に書いて", "前後の空白を落としていない");
    assert!(
        sent[0].3.starts_with("manual:"),
        "鍵は manual: 名前空間であるべき: {}",
        sent[0].3
    );
    // 監査のために出来事が 1 件残る。
    assert!(
        rt.events()
            .any(|e| e.kind == TeamEventKind::HumanInstruction),
        "人の指示が記録されていない"
    );
}

/// **空の指示は 1 バイトも送らない。** 空白だけも同じ。
#[test]
fn 空の指示は送らない() {
    let (mut rt, _, _, agent) = to_instructable();
    for text in ["", "   ", "\n\t "] {
        let out = rt.apply_action(TeamAction::InstructAgent {
            agent: AgentId(agent.clone()),
            text: text.into(),
        });
        assert!(manual_sends(&out).is_empty(), "空の指示が出た: {text:?}");
    }
}

/// **端末を持たない相手へは送らない。** ただし理由は残す
/// (押せるのに何も起きない、を記録の側でも作らない)。
#[test]
fn 端末が無い相手へは送らずに理由を残す() {
    // 起動前 — まだどの担当にもセッションが結び付いていない。
    let mut rt = started(4);
    let agent = rt.agents()[0].id.clone();
    assert!(
        rt.agents()[0].session_id.is_none(),
        "前提が崩れている (もう端末を持っている)"
    );
    let out = rt.apply_action(TeamAction::InstructAgent {
        agent: agent.clone(),
        text: "いまどう?".into(),
    });
    assert!(manual_sends(&out).is_empty(), "端末が無いのに送った: {out:?}");
    assert!(
        rt.events()
            .any(|e| e.kind == TeamEventKind::HumanInstruction),
        "送れなかったことが記録されていない"
    );
}

/// **同じ相手へ 2 回送ったら、鍵は必ず別になる。**
///
/// 同じ鍵だと 2 通目が「もう出した指示」として黙って落ちる。
#[test]
fn 人の指示の鍵は毎回変わる() {
    let (mut rt, _, _, agent) = to_instructable();
    let mut keys = Vec::new();
    for i in 0..5 {
        let out = rt.apply_action(TeamAction::InstructAgent {
            agent: AgentId(agent.clone()),
            text: format!("指示 {i}"),
        });
        let sent = manual_sends(&out);
        assert_eq!(sent.len(), 1, "{i} 通目が出ていない");
        keys.push(sent[0].3.clone());
    }
    let uniq: std::collections::BTreeSet<&String> = keys.iter().collect();
    assert_eq!(uniq.len(), keys.len(), "鍵が重複した: {keys:?}");
}

// ── 「いま送る」と「次の配布に足す」を混ぜない ───────────────────────
//
// 画面のボタンは 2 つあり、送り先が違う:
//
// * `InstructAgent` — 「いま送る」= 動いている端末へ 1 回だけ
// * `AddContext`    — 「次の配布に足す」= タスクの文脈へ
//
// **`InstructAgent` が文脈へも残すと、この区別が消える。** 一度きりの
// つもりで送った文言が、配り直した次の担当へも黙って渡ってしまう
// (人から見ると「取り消せない指示」になる)。

/// **今すぐ送る指示は、タスクの文脈へ 1 バイトも残らない。**
#[test]
fn 今すぐ送る指示はタスク文脈へ残らない() {
    let (mut rt, _, tid, agent) = to_instructable();
    let before = rt.task(tid).unwrap().context.clone();
    let out = rt.apply_action(TeamAction::InstructAgent {
        agent: AgentId(agent),
        text: "テストを先に書いて".into(),
    });
    assert_eq!(manual_sends(&out).len(), 1, "端末へ出ていない: {out:?}");
    assert!(
        !rt.task(tid)
            .unwrap()
            .context
            .iter()
            .any(|c| c.contains("テストを先に書いて")),
        "即時送信した指示がタスク文脈へ残っている"
    );
    assert_eq!(
        rt.task(tid).unwrap().context,
        before,
        "文脈が 1 行でも動いている"
    );
}

/// **次の配布に足す指示は、端末へは 1 通も出ない。**
///
/// 上のテストと対 — 片方だけだと「両方とも何もしない」実装が緑になる。
#[test]
fn 次の配布に足す指示は端末へ出ない() {
    let (mut rt, _, tid, _) = to_instructable();
    let out = rt.apply_action(TeamAction::AddContext {
        task: tid,
        text: "命名は snake_case で".into(),
    });
    assert!(
        manual_sends(&out).is_empty(),
        "文脈へ足しただけなのに端末へ送った: {out:?}"
    );
    assert!(
        rt.task(tid)
            .unwrap()
            .context
            .iter()
            .any(|c| c.contains("命名は snake_case で")),
        "タスクの文脈へ足されていない"
    );
}

/// **配り直した次の担当へ、その場の指示は渡らない。**
///
/// `context` を覗くだけの検査は「文脈の綴りを変えた」実装でも緑になる。
/// ここは**実際に配られる指示文**まで見る (積んだのに渡らない / 積んで
/// いないのに渡る、はどちらもこの層でしか出ない)。
///
/// 対照として `AddContext` の文言は**必ず渡る**ことも同時に見る。これが
/// 無いと、配り直しが起きていないだけの空振りが緑になってしまう。
#[test]
fn 今すぐ送る指示は次の担当へ渡らない() {
    let (mut rt, sids, tid) = to_assigned();
    let agent = rt.task(tid).unwrap().assigned_agent.clone().unwrap();
    rt.apply_action(TeamAction::AddContext {
        task: tid,
        text: "次の担当にも渡す約束".into(),
    });
    rt.apply_action(TeamAction::InstructAgent {
        agent,
        text: "この場かぎりの指示".into(),
    });

    // 担当の端末が消えた → 既存の回収経路で回収され、生きている別の
    // エージェントへ**同じ tick で**配り直される。
    let dead = rt.task(tid).unwrap().assigned_session.unwrap();
    let alive: Vec<SessionId> = sids.iter().copied().filter(|s| *s != dead).collect();
    let eff = idle_tick(&mut rt, 12, &alive);
    assert_ne!(
        rt.task(tid).unwrap().assigned_session,
        Some(dead),
        "回収されていない (前提が崩れている)"
    );
    let text = eff
        .iter()
        .find_map(|e| match e {
            TeamEffect::SendInstruction { task, text, .. } if *task == tid => Some(text.clone()),
            _ => None,
        })
        .expect("回収したタスクが配り直されない (前提が崩れている)");
    assert!(
        text.contains("次の担当にも渡す約束"),
        "「次の配布に足す」が指示文へ渡っていない:\n{text}"
    );
    assert!(
        !text.contains("この場かぎりの指示"),
        "「いま送る」で送った指示が、次の担当の指示文へ紛れている:\n{text}"
    );
}

/// **送信に失敗しても、タスクの文脈は汚れない。**
///
/// 「積めた = 届いた」ではないので、結末は遅れて戻る。そのどちらの道でも
/// 文脈を触らないことを見る (失敗の側だけ文脈へ書く実装を許さない)。
#[test]
fn 送信に失敗しても文脈は汚れない() {
    let (mut rt, _, tid, agent) = to_instructable();
    let before = rt.task(tid).unwrap().context.clone();
    let out = rt.apply_action(TeamAction::InstructAgent {
        agent: AgentId(agent),
        text: "届かない指示".into(),
    });
    let key = manual_sends(&out)[0].3.clone();
    rt.note_manual_delivery(&key, false, "宛先の端末が応答しませんでした");
    assert_eq!(
        rt.task(tid).unwrap().context,
        before,
        "配送に失敗した指示がタスク文脈へ残っている"
    );
    // **撃ち直さない。** 記録を捨てるので冪等キーは完了になっていない。
    assert!(
        !rt.effect_completed(&key),
        "届かなかった指示が完了として残っている"
    );
}

/// **監査では queued / delivered / failed が見分けられる。**
///
/// 発行の時点では配送は終わっていないので、そこへ「送りました」と書くと
/// この後 `queue_submit` が失敗しても記録は成功のままになる。
#[test]
fn 人の指示の記録は積んだ時点と届いた時点を分ける() {
    let (mut rt, _, _, agent) = to_instructable();
    let out = rt.apply_action(TeamAction::InstructAgent {
        agent: AgentId(agent.clone()),
        text: "先にテストを書いて".into(),
    });
    let key = manual_sends(&out)[0].3.clone();
    let queued: Vec<String> = human_instructions(&rt);
    assert_eq!(queued.len(), 1, "積んだ記録が 1 件ではない: {queued:?}");
    assert!(
        !queued[0].contains("送りました"),
        "まだ届いていないのに「送りました」と書いている: {}",
        queued[0]
    );
    assert!(
        queued[0].contains("キュー"),
        "積んだだけであることが記録から読めない: {}",
        queued[0]
    );

    // 届いた。
    rt.note_manual_delivery(&key, true, "");
    let done = human_instructions(&rt);
    assert_eq!(done.len(), 2, "結末が記録されていない: {done:?}");
    assert!(done[1].contains("届きました"), "{}", done[1]);
    assert!(rt.effect_completed(&key), "届いたのに完了になっていない");

    // 失敗の側は理由まで残る。
    let out = rt.apply_action(TeamAction::InstructAgent {
        agent: AgentId(agent),
        text: "もう 1 つ".into(),
    });
    let key = manual_sends(&out)[0].3.clone();
    rt.note_manual_delivery(&key, false, "送信キューへ積めませんでした");
    let all = human_instructions(&rt);
    let last = all.last().unwrap();
    assert!(
        last.contains("送れませんでした") && last.contains("送信キューへ積めませんでした"),
        "失敗の理由が記録から読めない: {last}"
    );
}

/// **鍵から宛先を読み直せる。** 名前に `:` が入っていても割れない
/// (前から切ると「エージェント名の途中」で割れる)。
#[test]
fn 人の指示の鍵から宛先を読み直せる() {
    for name in ["dev-1", "team:lead", "a:b:c"] {
        let id = AgentId(name.into());
        let key = manual_instruction_key(&id, 42);
        assert_eq!(
            manual_instruction_agent(&key),
            Some(id),
            "鍵から宛先を読めない: {key}"
        );
    }
    // Team のものではない目印は読まない (何も起きない)。
    for key in ["instr:1:dev-1:0:0", "manual:", "manual:dev-1", "start:dev-1"] {
        assert_eq!(manual_instruction_agent(key), None, "読めてはいけない: {key}");
    }
}

/// **空の指示は、送信も文脈更新もしない。**
#[test]
fn 空の指示は文脈も動かさない() {
    let (mut rt, _, tid, agent) = to_instructable();
    let before = rt.task(tid).unwrap().context.clone();
    let events_before = rt.events().count();
    for text in ["", "   ", "\n\t "] {
        let out = rt.apply_action(TeamAction::InstructAgent {
            agent: AgentId(agent.clone()),
            text: text.into(),
        });
        assert!(manual_sends(&out).is_empty(), "空の指示が出た: {text:?}");
    }
    assert_eq!(
        rt.task(tid).unwrap().context,
        before,
        "空の指示でタスク文脈が動いた"
    );
    assert_eq!(
        rt.events().count(),
        events_before,
        "空の指示で記録が増えた (押していないものを押したことにしている)"
    );
}

/// 人の指示として残った記録の本文を、古い順に取り出す。
fn human_instructions(rt: &TeamRuntime) -> Vec<String> {
    rt.events()
        .filter(|e| e.kind == TeamEventKind::HumanInstruction)
        .map(|e| e.summary.clone())
        .collect()
}

// ── 編成 (誰が何の担当か) ─────────────────────────────────────────────

/// **役割がそのまま担当と名前になる。**
///
/// 以前はリーダー以外が全員 `Implementer` の「Agent 1, 2, …」だったので、
/// 設計担当やテスト担当を選んでも画面には実装担当しか並ばず、
/// 「誰が何をする担当なのか」がどこにも出なかった。
#[test]
fn 編成は計画の役割から決まる() {
    use super::model::TeamRole as R;
    let t = |id: u64, role: R| {
        let mut x = super::testkit::task(id, "k", &[]);
        x.role = role;
        x
    };
    let tasks = vec![
        t(1, R::Architect),
        t(2, R::Implementer),
        t(3, R::Tester),
        t(4, R::Integrator),
    ];
    // 枠が足りていれば、計画にある役割が 1 体ずつ並ぶ。
    assert_eq!(
        roster_roles(&tasks, &[], 4),
        vec![R::Architect, R::Implementer, R::Tester, R::Integrator]
    );
    // 余った枠は実装へ寄せる (並列で効くのは実装なので)。
    assert_eq!(
        roster_roles(&tasks, &[], 6),
        vec![
            R::Architect,
            R::Implementer,
            R::Tester,
            R::Integrator,
            R::Implementer,
            R::Implementer
        ]
    );
    // 枠が足りなければ依存順に前から (設計が無いと実装が始まらない)。
    assert_eq!(roster_roles(&tasks, &[], 2), vec![R::Architect, R::Implementer]);
    assert!(roster_roles(&tasks, &[], 0).is_empty());
    // 役割の分からない計画でも、担当が 0 体にはならない。
    assert_eq!(roster_roles(&[], &[], 2), vec![R::Implementer, R::Implementer]);
}

/// **1 体しか居ない役割に番号を付けない** (存在しない 2 体目を探させる)。
#[test]
fn 担当の名前は役割から作る() {
    use super::model::TeamRole as R;
    assert_eq!(agent_name(R::Architect, 1), "Architect");
    assert_eq!(agent_name(R::Implementer, 2), "Implementer 2");
    assert_eq!(agent_name(R::TeamLead, 1), "Team Lead");
    // 役割は 7 つとも名前を持つ (増えたときに空欄が出ない)。
    for r in R::ALL {
        assert!(!agent_name(r, 1).is_empty(), "{r:?} に名前が無い");
    }
}

/// **段を挟んでも、チームは必要な人数ぶん最初から立つ。**
///
/// 実測 (0.23.0): 既定の役割を 6 つにしたら計画が
/// 「計画 → 設計 → 実装 8 件 → テスト → 統合」の段になり、
/// `desired_sessions` が「依存が空のタスク数」で数えていたせいで
/// **最後まで 2 体しか立たなかった** (盤面の「稼働 2」)。
/// `dependencies` は静的な項目なので、依存が済んでも空にはならない。
#[test]
fn 段のある計画でも並列ぶんの担当が立つ() {
    use super::graph::max_parallel_width;
    use super::model::TeamRole as R;
    let mut tasks = Vec::new();
    // 計画 (1) → 設計 (2) → 実装 8 件 → テスト → 統合
    let mut plan = super::testkit::task(1, "plan", &[]);
    plan.role = R::Planner;
    tasks.push(plan);
    let mut design = super::testkit::task(2, "design", &[1]);
    design.role = R::Architect;
    tasks.push(design);
    for i in 0..8u64 {
        let mut t = super::testkit::task(10 + i, "impl", &[2]);
        t.role = R::Implementer;
        tasks.push(t);
    }
    let impls: Vec<u64> = (0..8u64).map(|i| 10 + i).collect();
    let mut test = super::testkit::task(30, "test", &impls);
    test.role = R::Tester;
    tasks.push(test);
    let mut integ = super::testkit::task(31, "integrate", &[30]);
    integ.role = R::Integrator;
    tasks.push(integ);

    // いちばん広い段は実装の 8 件。
    assert_eq!(max_parallel_width(&tasks), 8, "段の幅を取り違えている");
    // 上限 4 なら 4 体 (上限 12 なら レビュー用の 1 体を足して 9 体)。
    assert_eq!(super::scheduler::desired_sessions(&tasks, 4), 4);
    assert_eq!(super::scheduler::desired_sessions(&tasks, 12), 9);
    // **直った証拠**: 旧い数え方 (依存が空) だと 1 件しかない。
    let old_way = tasks
        .iter()
        .filter(|t| t.dependencies.is_empty() && !t.state.is_terminal())
        .count();
    assert_eq!(old_way, 1, "旧い数え方では 1 = 2 体しか立たなかった");

    // 終わった段は数に入れない (済んだぶん余分に立ち続けない)。
    for t in tasks.iter_mut().filter(|t| t.role == R::Implementer) {
        t.state = TeamTaskState::Completed;
    }
    assert_eq!(max_parallel_width(&tasks), 1, "残っているのは 1 段 1 件");
}

// ── エージェント同士のやり取り ───────────────────────────────────────

/// **伝言は「言った」だけで終わらせない。相手の端末へ実際に届く。**
///
/// 端末の中だけで完結すると盤面に何も残らず、受け手も気付かない。
/// `tick` を通して、(1) 出来事として残り (2) 配達の Effect が出ることを見る。
#[test]
fn エージェントの伝言は相手の端末へ届く() {
    let mut rt = started(4);
    // 端末が結び付いていないと伝言は配れない (偽の起動で結び付ける)。
    let mut next: SessionId = 1;
    let boot = rt.tick(&obs_for_test(1, &[]));
    let bound = bind_all(&mut rt, &boot, &mut next);
    let ids: Vec<AgentId> = rt
        .agents()
        .iter()
        .filter(|a| a.kind == AgentKind::ManagedSession)
        .map(|a| a.id.clone())
        .collect();
    assert!(ids.len() >= 2, "2 体以上居ないと伝言の相手が居ない");
    let (from, to) = (ids[0].clone(), ids[1].clone());
    let from_sid = rt.agent(&from).and_then(|a| a.session_id).expect("端末");
    let to_sid = rt.agent(&to).and_then(|a| a.session_id).expect("端末");

    let screen = format!(
        "{}\n{{\"to\": \"{}\", \"text\": \"設計が終わった。次は実装に入って\"}}\n{}",
        rp::MSG_OPEN, to.0, rp::MSG_CLOSE
    );
    // **全セッションを載せる。** 載せないと、載せなかったぶんは
    // 「消えた」と解釈されて端末が外れる (受け手が居なくなる)。
    let mut obs = obs_for_test(100, &bound);
    for s in &mut obs.sessions {
        if s.id == from_sid {
            s.text = screen.clone();
        }
    }
    let effects = rt.tick(&obs);
    // (1) 出来事として残る (あとから誰が誰に何を言ったか追える)。
    let logged = rt
        .events()
        .find(|e| e.kind == TeamEventKind::AgentMessage)
        .expect("伝言が出来事に残っていない");
    assert_eq!(logged.actor.as_ref(), Some(&from));
    assert_eq!(logged.target.as_ref(), Some(&to));
    assert!(logged.summary.contains("次は実装に入って"), "{}", logged.summary);

    // (2) 相手の端末へ配る Effect が出る。
    let sent = effects
        .iter()
        .find_map(|e| match e {
            TeamEffect::SendManualInstruction {
                agent,
                session,
                text,
                ..
            } if agent == &to => Some((*session, text.clone())),
            _ => None,
        })
        .expect("配達の Effect が出ていない");
    assert_eq!(sent.0, to_sid, "別の端末へ配ろうとしている");
    assert!(sent.1.contains("次は実装に入って"), "{}", sent.1);
    assert!(sent.1.contains("からの伝言"), "差出人が本文に無い: {}", sent.1);
}

/// **居ない相手への伝言は断る。** 通すと盤面には「伝えた」と出るのに
/// 誰も受け取っていない、という嘘になる。
#[test]
fn 居ない相手への伝言は断る() {
    let mut rt = started(4);
    let mut next: SessionId = 1;
    let boot = rt.tick(&obs_for_test(1, &[]));
    let bound = bind_all(&mut rt, &boot, &mut next);
    let from = rt
        .agents()
        .iter()
        .find(|a| a.kind == AgentKind::ManagedSession)
        .map(|a| a.id.clone())
        .expect("担当");
    let sid = rt.agent(&from).and_then(|a| a.session_id).expect("端末");
    let screen = format!(
        "{}\n{{\"to\": \"だれか\", \"text\": \"やあ\"}}\n{}",
        rp::MSG_OPEN, rp::MSG_CLOSE
    );
    let mut obs = obs_for_test(100, &bound);
    for s in &mut obs.sessions {
        if s.id == sid {
            s.text = screen.clone();
        }
    }
    let effects = rt.tick(&obs);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, TeamEffect::SendManualInstruction { .. })),
        "居ない相手へ配ろうとしている"
    );
    assert!(
        rt.events()
            .any(|e| e.kind == TeamEventKind::Rejected && e.summary.contains("居ません")),
        "断った理由が残っていない"
    );
}

/// **自分宛ては配らない。** 自分の端末へ自分の言葉を流しても何も起きない。
#[test]
fn allは自分を含めない() {
    let known: Vec<(AgentId, String)> = vec![
        (AgentId::new("a"), "implementer".into()),
        (AgentId::new("b"), "reviewer".into()),
    ];
    let body = "{\"to\": \"all\", \"text\": \"できた\"}";
    let (targets, _) = rp::check_message(body, &known, &AgentId::new("a")).unwrap();
    assert_eq!(targets, vec![AgentId::new("b")]);
    // 役割でも宛てられる。
    let body = "{\"to\": \"reviewer\", \"text\": \"見て\"}";
    let (targets, _) = rp::check_message(body, &known, &AgentId::new("a")).unwrap();
    assert_eq!(targets, vec![AgentId::new("b")]);
}

/// **混んでいるだけのレビューを人の判断待ちにしない。**
///
/// 実機で「実装した本人しかレビュー候補がいません」が 5 件積み上がり、
/// 再試行を押しても消えなかった。原因は 2 つ:
/// 1. **空集合へ `all` を撃っていた** — 誰も空いていないときも真になるので、
///    ただ混んでいるだけのレビューが「本人しか居ない」に化けていた
/// 2. 化けた札は `Ready` のまま積まれるので、`retry` が状態を動かさず
///    掃除もされない (= 押しても消えない)
#[test]
fn 混んでいるだけのレビューは人へ上げない() {
    // 2 体で回すと、1 本目のレビューが生まれた時点で **もう 1 体は別の
    // 実装を握っている** ので、空いているのは著者だけ — 実機と同じ局面。
    let mut rt = started(2);
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
    for now in 13..18 {
        idle_tick(&mut rt, now, &sids);
    }
    // **局面が作れたことを先に確かめる。** 作れていないと、この下の
    // アサートは何も守らない (空回りするテストを残さない)。
    let rev = rt
        .tasks()
        .iter()
        .find(|t| t.review_of == Some(tid))
        .expect("レビュータスクが生まれていない");
    assert_eq!(
        rev.state,
        TeamTaskState::Ready,
        "レビューが配れてしまい、混雑の局面になっていない"
    );
    assert!(
        rt.tasks()
            .iter()
            .any(|t| t.assigned_session.is_some_and(|x| x != sid) && t.state.is_held()),
        "もう 1 体が空いている (混雑の局面になっていない)"
    );
    let raised: Vec<&str> = rt
        .decisions()
        .iter()
        .filter(|d| d.kind == DecisionKind::NoCandidate)
        .map(|d| d.reason.as_str())
        .collect();
    assert!(
        raised.is_empty(),
        "混んでいるだけで人の判断待ちが出た: {raised:?}"
    );
}

/// **レビュー役が本当に居ないときは、1 件だけ出して自分で消す。**
///
/// 1 体だけの Run では実装した本人しか居ないので、レビューは待っても
/// 配れない。人に伝える価値があるのはこちらだけ。
#[test]
fn レビュー役が居ないときは一度だけ上げて再試行で下ろせる() {
    let mut rt = started(1);
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
    // 何 tick 回しても **1 件のまま** (毎 tick 積み増さない)。
    for now in 13..18 {
        idle_tick(&mut rt, now, &sids);
    }
    let stuck: Vec<(TaskId, u64)> = rt
        .decisions()
        .iter()
        .filter(|d| d.kind == DecisionKind::NoCandidate)
        .map(|d| (d.task_id.unwrap_or(0), d.id))
        .collect();
    assert_eq!(stuck.len(), 1, "レビュー役不在の札が増えている: {stuck:?}");
    let rev = stuck[0].0;
    // **再試行を押したら、その場で消える。**
    // 押しても消えないと、人からは「壊れている」としか見えない。
    rt.apply_action(TeamAction::RetryTask(rev));
    assert!(
        !rt.decisions()
            .iter()
            .any(|d| d.task_id == Some(rev) && d.kind == DecisionKind::NoCandidate),
        "再試行を押しても札が残っている"
    );
}

/// **配置から導く判断は、全部「取り下げの対象」に載せる。**
///
/// 載せ忘れると、理由が消えたあとも札だけが画面に残る (そして `retry` でも
/// 消えないので、人には壊れているようにしか見えない)。
#[test]
fn 配置から導く判断はすべて取り下げの対象() {
    let src = include_str!("runtime.rs").replace("\r\n", "\n");
    let body = src
        .split("fn dispatch(&mut self")
        .nth(1)
        .and_then(|t| t.split("\n    /// ").next())
        .expect("dispatch がある");
    // `dispatch` が `standing` へ入れる鍵は、必ず表の頭で始まること。
    let mut seen = 0usize;
    for line in body.lines() {
        let l = line.trim_start();
        if l.starts_with("//") || !l.contains("standing.insert(format!(\"") {
            continue;
        }
        seen += 1;
        let key = l.split("format!(\"").nth(1).and_then(|t| t.split('{').next());
        let key = key.expect("鍵の頭が読める");
        assert!(
            super::runtime::SCHEDULING_KEYS.contains(&key),
            "{key:?} が SCHEDULING_KEYS に無い (理由が消えても札が残る)"
        );
    }
    assert!(seen >= 3, "配置由来の判断を読み落としている (seen={seen})");
}

/// **伝言で担当を投げ出させない。**
///
/// 実機で index.html を書く担当 (#5) が 1 時間止まった。伝言で CSS の
/// 手直しへ移ってしまい、**ページの本体が最後まで作られなかった**
/// (`~/dev/Test5` に css / js / docs はあるのに index.html が無い)。
/// 相手の端末には「連絡」と「指示」の区別が無いので、こちらが毎回書く。
#[test]
fn 伝言には担当が変わっていないことを添える() {
    let (mut rt, sids, tid) = to_assigned();
    let to = rt.task(tid).unwrap().assigned_agent.clone().unwrap();
    let from = rt
        .agents()
        .iter()
        .map(|a| a.id.clone())
        .find(|a| a != &to)
        .expect("差出人");
    let sender = from.0.clone();
    let body = format!(
        "{}\n{{\"to\": \"{}\", \"text\": \"CSS の手直しをお願いします\"}}\n{}",
        rp::MSG_OPEN,
        to.0,
        rp::MSG_CLOSE
    );
    let from_sid = rt
        .agents()
        .iter()
        .find(|a| a.id == from)
        .and_then(|a| a.session_id)
        .expect("差出人の端末");
    let eff = tick_text(&mut rt, 30, &sids, from_sid, &body);
    let sent = eff
        .iter()
        .find_map(|e| match e {
            TeamEffect::SendManualInstruction { agent, text, .. } if agent == &to => Some(text),
            _ => None,
        })
        .unwrap_or_else(|| panic!("伝言が届いていない (差出人 {sender})"));
    assert!(sent.contains("CSS の手直し"), "本文が消えた");
    assert!(
        sent.contains(&format!("#{tid}")),
        "いまの担当が添えられていない: {sent}"
    );
    assert!(
        sent.contains("連絡") && sent.contains("指示ではありません"),
        "連絡か指示かが区別できない: {sent}"
    );
}

/// **書いている途中の画面から報告を断らない。**
///
/// 実機 (Test6) で、担当が正しく報告しているのに
/// `報告の JSON を読めません: invalid type: string "task_id"` が記録された。
/// 1 tick が描画の途中に当たり、マーカーの間に `"task_id"` の断片しか
/// 無かったため。断ると、落ち度の無い担当に却下が積まれ、人には
/// 「エージェントが壊れた報告を出した」ように見える。
#[test]
fn 書き途中の報告は断らずに見送る() {
    let (mut rt, sids, tid) = to_assigned();
    let (sid, _, _) = assignments(&rt)[0].clone();
    let before = rt.events().count();
    // 描画の途中: 中身の断片しか無い。
    // **指示文には無い断片**を使う (指示文に含まれる断片は、既存の
    // エコー除去が先に落としてしまい、この番人が何も守らなくなる)。
    let half = format!(
        "{}\n  \"summary\": \"3D canvas と Hero を実装\",\n{}",
        rp::RESULT_OPEN,
        rp::RESULT_CLOSE
    );
    tick_text(&mut rt, 20, &sids, sid, &half);
    let rejected: Vec<String> = rt
        .events()
        .skip(before)
        .filter(|e| e.kind == TeamEventKind::Rejected)
        .map(|e| e.summary.clone())
        .collect();
    assert!(
        rejected.is_empty(),
        "書き途中の画面で却下が記録された: {rejected:?}"
    );
    // **次の tick で全部揃えば、これまでどおり受け付ける。**
    let agent = rt.task(tid).unwrap().assigned_agent.clone().unwrap().0;
    let files: Vec<String> = rt.task(tid).unwrap().files.clone();
    let fs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    let full = result_block(tid, &agent, &["cargo test auth"], &fs);
    tick_text(&mut rt, 21, &sids, sid, &full);
    assert_ne!(
        rt.task(tid).unwrap().state,
        TeamTaskState::Running,
        "揃った報告まで見送っている"
    );
}

/// **同じ断りで台帳を埋めない。**
///
/// 実測: 調停層の断り (`割り当て可能なセッションがいない`) が 2 秒ごとに
/// 2 件積まれ、**台帳 500 件がそれだけ**になった。計画も起動も伝言も
/// 押し出され、人には何が起きたか一切追えなくなった。
/// 断りは配置から導かれるので、配置が変わるまで同じ行が出続ける。
///
/// 振る舞いで固定しようとしたが、**調停層に断らせる局面を作れず空回りした**
/// (規則を 1 本に揃えた結果、`NoEligibleCandidate` が起きにくくなった)。
/// 局面を作れないものを振る舞いで書くと「通っているのに何も守らない」
/// テストになるので、**記録の入口が覚え書きを通っていること**を走査で見る。
#[test]
fn 同じ断りを毎tick記録しない() {
    let src = include_str!("runtime.rs").replace("\r\n", "\n");
    let at = src
        .find("の割り当てを見送りました")
        .expect("断りの記録場所がある");
    // その直前 (同じ腕の中) に覚え書きの門があること。
    let head = &src[at.saturating_sub(400)..at];
    assert!(
        head.contains("blocked_notes.insert("),
        "断りが毎 tick 記録される (覚え書きの門を通っていない)"
    );
    // 覚え書きは保存しない (`previews` / `stalls` と同じ扱い)。
    assert!(
        !src.contains("blocked_notes:") || !src.contains("serde(default)]\n    blocked_notes"),
        "覚え書きを永続化している"
    );
}
