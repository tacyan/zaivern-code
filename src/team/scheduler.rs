//! 決定的スケジューラ — **誰にどのタスクを配るかは LLM が決めない。**
//!
//! ## なぜ決定的でなければならないか
//!
//! 割り当てが確率的だと、同じ状態から違う結果が出る = 再現できない。
//! 「64 体で 18/64 しか通らない」のような不具合は、再現できて初めて直せる。
//! だからここは**同じ入力に必ず同じ出力**を返す純関数にする。
//!
//! ## 優先順位 (仕様どおり・テストで固定)
//!
//! 1. 依存関係が解決済み (Ready であること)
//! 2. file scope が他タスクと競合しない → **判定は既存の
//!    [`crate::coordinator::admit`] に任せる** (自前で作らない)
//! 3. `required_caps` に合致する
//! 4. Idle なエージェントを優先する
//! 5. クリティカルパス上のタスクを優先する
//! 6. Task ID 順
//!
//! ## 実装担当とレビュアーを同じセッションにしない
//!
//! 自分の書いたコードを自分で承認できてしまうと、レビューが儀式になる。
//! [`plan_assignments`] はレビュータスクの候補から、**その実装タスクを
//! 担当したセッション**を必ず外す。

use std::collections::{BTreeMap, BTreeSet};

use crate::coordinator::{SessionInfo, SessionState};

use super::model::{AgentId, TaskId, TeamTask, TeamTaskState};

/// 割り当て候補となるエージェント 1 体の要約。
///
/// `coordinator::SessionInfo` に Team 側の情報 (エージェント ID・保有タスク)
/// を足したもの。**セッションの真実は既存側**なので、状態はそのまま持つ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub agent: AgentId,
    pub session: crate::coordinator::SessionId,
    pub state: SessionState,
    /// 申告している能力 (小文字)。
    pub caps: Vec<String>,
    /// いま抱えているタスク (空なら空き)。
    pub holding: Option<TaskId>,
}

impl Candidate {
    /// 既存調停層へ渡す姿。
    pub fn as_info(&self) -> SessionInfo {
        let caps: Vec<&str> = self.caps.iter().map(|s| s.as_str()).collect();
        SessionInfo::new(self.session, self.state, &caps)
    }

    /// いま新しい仕事を受けられるか。
    fn free(&self) -> bool {
        self.holding.is_none() && matches!(self.state, SessionState::Idle | SessionState::Working)
    }
}

/// スケジューラが出す 1 件の割り当て案。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    pub task: TaskId,
    pub agent: AgentId,
    pub session: crate::coordinator::SessionId,
}

/// 割り当てられなかった理由 (人へ出す・イベントに残す)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unassigned {
    /// 条件に合う空きエージェントがいない。
    NoCandidate(TaskId),
    /// 能力が足りない。
    CapsMissing { task: TaskId, caps: Vec<String> },
    /// 担当ファイルが他タスクと重なる。
    FileOverlap { task: TaskId, with: TaskId },
    /// レビュー担当が実装担当と同じになってしまう。
    ReviewerWouldBeAuthor(TaskId),
}

impl Unassigned {
    pub fn task(&self) -> TaskId {
        match self {
            Unassigned::NoCandidate(t)
            | Unassigned::CapsMissing { task: t, .. }
            | Unassigned::FileOverlap { task: t, .. }
            | Unassigned::ReviewerWouldBeAuthor(t) => *t,
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Unassigned::NoCandidate(t) => format!("#{t}: 割り当て可能なエージェントがいません"),
            Unassigned::CapsMissing { task, caps } => {
                format!(
                    "#{task}: 必要な能力 {} を持つエージェントがいません",
                    caps.join(", ")
                )
            }
            Unassigned::FileOverlap { task, with } => {
                format!("#{task}: 担当ファイルが #{with} と重なるため配りません")
            }
            Unassigned::ReviewerWouldBeAuthor(t) => {
                format!("#{t}: 実装した本人しかレビュー候補がいません")
            }
        }
    }
}

/// スケジューラの結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub assignments: Vec<Assignment>,
    pub unassigned: Vec<Unassigned>,
}

/// 担当ファイルのパターンが重なるか。
///
/// **判定の本体は既存の [`crate::lease::overlaps`] を使う** — glob・末尾
/// スラッシュ・Windows の大小畳み込み・行域まで面倒を見ている 1 本で、
/// `coordinator::admit` とフックもこれを通る。ここで別実装を作ると、
/// スケジューラが「行ける」と言ったものを `coordinator` が断る
/// (またはその逆) というズレが必ず出る。
fn files_overlap(a: &[String], b: &[String]) -> Option<()> {
    for x in a {
        for y in b {
            if crate::lease::overlaps(x, y) {
                return Some(());
            }
        }
    }
    None
}

/// 能力が足りているか。タスクが要求する `required_caps` を全部持っているか。
fn caps_ok(task: &TeamTask, c: &Candidate) -> bool {
    task.required_caps
        .iter()
        .all(|need| c.caps.iter().any(|have| have == need))
}

/// 割り当て案を作る (純関数・決定的)。
///
/// `tasks` は全タスク (状態を見て Ready のものだけ配る)。
/// `candidates` は生きているエージェント。
/// `depth` は [`super::graph::critical_depth`] の結果。
pub fn plan_assignments(
    tasks: &[TeamTask],
    candidates: &[Candidate],
    depth: &BTreeMap<TaskId, u32>,
) -> Plan {
    let mut out = Plan::default();

    // 配る順: クリティカルパスの深い順 → ID 昇順。**同点は必ず ID で割る**
    // ので、同じ入力に同じ順序が出る。
    let mut ready: Vec<&TeamTask> = tasks
        .iter()
        .filter(|t| t.state == TeamTaskState::Ready)
        .collect();
    ready.sort_by(|a, b| {
        // **レビューを先に配る。**
        //
        // 後回しにすると、空いている担当が新しい実装で埋まってしまい、
        // レビューの番が来たときには**実装した本人しか残っていない**
        // (自分のレビューは禁止なので配れない)。実機ではこれで 8 件が
        // 「実装した本人しかレビュー候補がいません」で人の判断待ちになり、
        // Goal ごと止まった。
        //
        // レビューは実装より短く、終われば担当が空くので、先に通すほうが
        // 全体も速い。
        let ra = u8::from(a.review_of.is_none());
        let rb = u8::from(b.review_of.is_none());
        let da = depth.get(&a.id).copied().unwrap_or(0);
        let db = depth.get(&b.id).copied().unwrap_or(0);
        ra.cmp(&rb).then(db.cmp(&da)).then(a.id.cmp(&b.id))
    });

    // すでに誰かが握っているファイル (このフレームで配ったぶんも足す)。
    let mut held: Vec<(TaskId, Vec<String>)> = tasks
        .iter()
        .filter(|t| t.state.is_held())
        .map(|t| (t.id, t.files.clone()))
        .collect();

    // このフレームで使ったエージェント。
    let mut taken: BTreeSet<crate::coordinator::SessionId> = candidates
        .iter()
        .filter(|c| !c.free())
        .map(|c| c.session)
        .collect();

    for t in ready {
        // 2) file scope の重なり。**配る前に止める** (fail-closed)。
        if let Some((with, _)) = held
            .iter()
            .find(|(id, files)| *id != t.id && files_overlap(&t.files, files).is_some())
        {
            out.unassigned.push(Unassigned::FileOverlap {
                task: t.id,
                with: *with,
            });
            continue;
        }

        // レビュータスクは、実装した本人のセッションを外す。
        let author_session = t
            .review_of
            .and_then(|src| tasks.iter().find(|x| x.id == src))
            .and_then(|x| x.assigned_session);

        let mut pool: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| c.free() && !taken.contains(&c.session))
            .filter(|c| Some(c.session) != author_session)
            .collect();

        if pool.is_empty() {
            // 候補が居ないのか、本人しか居ないのかを区別して伝える。
            let only_author = author_session.is_some()
                && candidates
                    .iter()
                    .filter(|c| c.free() && !taken.contains(&c.session))
                    .all(|c| Some(c.session) == author_session);
            out.unassigned.push(if only_author {
                Unassigned::ReviewerWouldBeAuthor(t.id)
            } else {
                Unassigned::NoCandidate(t.id)
            });
            continue;
        }

        // 3) 能力での絞り込み。
        let capable: Vec<&Candidate> = pool.iter().copied().filter(|c| caps_ok(t, c)).collect();
        if capable.is_empty() {
            out.unassigned.push(Unassigned::CapsMissing {
                task: t.id,
                caps: t.required_caps.clone(),
            });
            continue;
        }
        pool = capable;

        // 4) Idle 優先 → 合致した能力の多い順 → セッション ID 昇順。
        pool.sort_by(|a, b| {
            let idle = |c: &Candidate| u8::from(c.state != SessionState::Idle);
            let matched = |c: &Candidate| {
                t.required_caps
                    .iter()
                    .filter(|need| c.caps.iter().any(|h| h == *need))
                    .count()
            };
            idle(a)
                .cmp(&idle(b))
                .then(matched(b).cmp(&matched(a)))
                .then(a.session.cmp(&b.session))
        });

        let chosen = pool[0];
        taken.insert(chosen.session);
        held.push((t.id, t.files.clone()));
        out.assignments.push(Assignment {
            task: t.id,
            agent: chosen.agent.clone(),
            session: chosen.session,
        });
    }

    out
}

/// `--agents N` から、実際に起動してよい ManagedSession 数を決める。
///
/// **無条件に N 体起こさない。** 計画に含まれる同時実行可能なタスク数
/// (依存の無いタスク数) を上限にする — 4 と言われても仕事が 1 つしか
/// 無ければ 1 体でよい。
pub fn desired_sessions(tasks: &[TeamTask], max_agents: usize) -> usize {
    if max_agents == 0 {
        return 0;
    }
    // **同時に走りうる最大数**で見る。
    //
    // 以前は「依存が空のタスク数」で数えていたが、`dependencies` は
    // 静的な項目なので、依存が済んでも空にはならない。段を 1 つでも
    // 挟んだ計画では永久に 1 になり、実装が 8 件並べるのに**最後まで
    // 2 体しか立たなかった** (実測: 盤面の「稼働 2」)。
    //
    // 幅で見れば、チームは最初から必要な人数ぶん立ち上がる —
    // 上の段を待っている間も居るので、順番が来た瞬間に一斉に動ける。
    let parallel = super::graph::max_parallel_width(tasks);
    // **レビュー用に 1 体余分に見る。** 実装担当は自分のレビューをできない
    // ので、並列実装数ぴったりだと「全員が実装中でレビューできない」状態が
    // 生まれ、上限に当たるまで誰も先へ進めない (実測で詰まった)。
    let need = if tasks.len() > 1 {
        parallel.max(1) + 1
    } else {
        1
    };
    need.min(max_agents)
}

#[cfg(test)]
mod tests {
    use super::super::testkit::task;
    use super::*;

    fn cand(id: u64, state: SessionState, caps: &[&str]) -> Candidate {
        Candidate {
            agent: AgentId::new(format!("a{id}")),
            session: id,
            state,
            caps: caps.iter().map(|s| s.to_string()).collect(),
            holding: None,
        }
    }

    fn ready(id: TaskId, key: &str, files: &[&str]) -> TeamTask {
        let mut t = task(id, key, &[]);
        t.state = TeamTaskState::Ready;
        t.files = files.iter().map(|s| s.to_string()).collect();
        t
    }

    #[test]
    fn 同じ入力なら同じ割り当て() {
        let tasks = vec![
            ready(1, "a", &["src/a.rs"]),
            ready(2, "b", &["src/b.rs"]),
            ready(3, "c", &["src/c.rs"]),
        ];
        let cands = vec![
            cand(3, SessionState::Idle, &[]),
            cand(1, SessionState::Idle, &[]),
            cand(2, SessionState::Idle, &[]),
        ];
        let d = BTreeMap::new();
        let a = plan_assignments(&tasks, &cands, &d);
        let b = plan_assignments(&tasks, &cands, &d);
        assert_eq!(a, b);
        // ID 昇順で配る (同点は必ず ID)
        assert_eq!(a.assignments[0].session, 1);
        assert_eq!(a.assignments[1].session, 2);
        assert_eq!(a.assignments[2].session, 3);
    }

    #[test]
    fn ファイルが重なるタスクは同時に配らない() {
        let mut held = ready(1, "a", &["src/auth/**"]);
        held.state = TeamTaskState::Running;
        let overlap = ready(2, "b", &["src/auth/login.rs"]);
        let tasks = vec![held, overlap];
        let cands = vec![
            cand(1, SessionState::Idle, &[]),
            cand(2, SessionState::Idle, &[]),
        ];
        let p = plan_assignments(&tasks, &cands, &BTreeMap::new());
        assert!(p.assignments.is_empty(), "{:?}", p.assignments);
        assert_eq!(
            p.unassigned,
            vec![Unassigned::FileOverlap { task: 2, with: 1 }]
        );
    }

    #[test]
    fn 同じフレーム内でも重なりを止める() {
        let tasks = vec![
            ready(1, "a", &["src/auth/**"]),
            ready(2, "b", &["src/auth/login.rs"]),
        ];
        let cands = vec![
            cand(1, SessionState::Idle, &[]),
            cand(2, SessionState::Idle, &[]),
        ];
        let p = plan_assignments(&tasks, &cands, &BTreeMap::new());
        assert_eq!(p.assignments.len(), 1);
        assert_eq!(p.assignments[0].task, 1);
        assert_eq!(p.unassigned.len(), 1);
    }

    #[test]
    fn 能力が足りないと配らない() {
        let mut t = ready(1, "a", &["src/a.rs"]);
        t.required_caps = vec!["rust".into()];
        let cands = vec![cand(1, SessionState::Idle, &["python"])];
        let p = plan_assignments(&[t], &cands, &BTreeMap::new());
        assert!(p.assignments.is_empty());
        assert!(matches!(p.unassigned[0], Unassigned::CapsMissing { .. }));
    }

    #[test]
    fn 実装担当はレビュー担当になれない() {
        let mut impl_t = task(1, "a", &[]);
        impl_t.state = TeamTaskState::Reviewing;
        impl_t.assigned_session = Some(1);
        let mut rev = ready(2, "rev", &[]);
        rev.review_of = Some(1);
        let tasks = vec![impl_t, rev];
        // 空きが実装担当 (session 1) だけ → 配らない
        let only_author = vec![cand(1, SessionState::Idle, &[])];
        let p = plan_assignments(&tasks, &only_author, &BTreeMap::new());
        assert_eq!(p.unassigned, vec![Unassigned::ReviewerWouldBeAuthor(2)]);
        // 別のセッションが居れば、そちらへ配る
        let two = vec![
            cand(1, SessionState::Idle, &[]),
            cand(2, SessionState::Idle, &[]),
        ];
        let p2 = plan_assignments(&tasks, &two, &BTreeMap::new());
        assert_eq!(p2.assignments.len(), 1);
        assert_eq!(p2.assignments[0].session, 2);
    }

    #[test]
    fn クリティカルパスを優先する() {
        let tasks = vec![ready(1, "a", &["src/a.rs"]), ready(2, "b", &["src/b.rs"])];
        let mut d = BTreeMap::new();
        d.insert(1u64, 0u32);
        d.insert(2u64, 5u32);
        let cands = vec![cand(1, SessionState::Idle, &[])];
        let p = plan_assignments(&tasks, &cands, &d);
        assert_eq!(p.assignments.len(), 1);
        assert_eq!(p.assignments[0].task, 2, "深い方を先に配るべき");
    }

    #[test]
    fn idleを優先する() {
        let tasks = vec![ready(1, "a", &["src/a.rs"])];
        let cands = vec![
            cand(1, SessionState::Working, &[]),
            cand(2, SessionState::Idle, &[]),
        ];
        let p = plan_assignments(&tasks, &cands, &BTreeMap::new());
        assert_eq!(p.assignments[0].session, 2);
    }

    #[test]
    fn 保有中や停滞中のセッションへは配らない() {
        let tasks = vec![ready(1, "a", &["src/a.rs"])];
        let mut busy = cand(1, SessionState::Idle, &[]);
        busy.holding = Some(9);
        let cands = vec![
            busy,
            cand(2, SessionState::Stalled, &[]),
            cand(3, SessionState::Exited, &[]),
        ];
        let p = plan_assignments(&tasks, &cands, &BTreeMap::new());
        assert!(p.assignments.is_empty());
        assert_eq!(p.unassigned, vec![Unassigned::NoCandidate(1)]);
    }

    #[test]
    fn readyでないタスクは配らない() {
        let mut t = task(1, "a", &[]);
        t.state = TeamTaskState::Pending;
        let p = plan_assignments(&[t], &[cand(1, SessionState::Idle, &[])], &BTreeMap::new());
        assert!(p.assignments.is_empty());
        assert!(p.unassigned.is_empty());
    }

    #[test]
    fn 起動数は計画に必要なぶんだけ() {
        let t1 = task(1, "a", &[]);
        assert_eq!(
            desired_sessions(&[t1.clone()], 4),
            1,
            "仕事が 1 つなら 1 体"
        );
        let t2 = task(2, "b", &[1]);
        assert_eq!(
            desired_sessions(&[t1.clone(), t2], 4),
            2,
            "レビュー用に最低 2 体"
        );
        let many: Vec<TeamTask> = (1..=8).map(|i| task(i, &format!("k{i}"), &[])).collect();
        assert_eq!(desired_sessions(&many, 4), 4, "上限を超えない");
        assert_eq!(desired_sessions(&many, 0), 0);
    }

    #[test]
    fn エージェント0体でも壊れない() {
        let tasks = vec![ready(1, "a", &["src/a.rs"])];
        let p = plan_assignments(&tasks, &[], &BTreeMap::new());
        assert!(p.assignments.is_empty());
        assert_eq!(p.unassigned, vec![Unassigned::NoCandidate(1)]);
    }
}
