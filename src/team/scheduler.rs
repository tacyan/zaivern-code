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
    ///
    /// **持ち仕事が無いなら空いている。** ここが `Idle | Working` だけを
    /// 見ていたので、実機で**空いている担当が 3 体居るのにタスクが 5 本
    /// 待ち続ける**という止まり方をした (Planner / Tester / Reviewer が
    /// `stalled`、#1 #2 #10 #11 #14 が `ready` のまま)。
    ///
    /// 状態は**画面から推し量った値**である。仕事を持っていない担当は
    /// 出力が動かなくて当たり前なので、そのまま「停滞」と読まれる。
    /// そして停滞と読まれた担当には配らないので、**二度と出力が動かない** —
    /// 推測が自分で自分を裏付ける輪になっていた。
    ///
    /// 配らない理由になるのは、**推し量らなくても分かる 2 つ**だけ:
    ///
    /// * `Exited` — プロセスが居ない。曖昧さが無い
    /// * `WaitingApproval` — 承認の返事として本文が解釈される。**絶対に
    ///   注入しない** (`coordinator::SessionState` の doc を参照)
    ///
    /// 残りは配ってよい。**打ち込んでよい頃合いかどうかは `submit` が
    /// 別に見ている** (入力欄の準備を待ち、駄目なら人へ返す) ので、
    /// ここで二重に見張って止まるより、渡して確かめるほうが速い。
    /// 設計原則 4「エージェントの状態を画面から推測しない」の具体形 —
    /// **何も配っていないという構造的な事実**のほうが、画面の読みより強い。
    fn free(&self) -> bool {
        // **配ってよいかの判定は調停層の 1 本を借りる。**
        //
        // ここに別の規則を書くと、**提案しては断られる**組み合わせが生まれ、
        // 毎 tick 「割り当てを見送りました」が記録される (実測で台帳が
        // 500 件のそれだけで埋まった)。
        //
        // 「仕事を持っていない担当が停滞に見えて二度と配られない」問題は、
        // ここを緩めて解くのではなく **`roles::derive_agent_work_state` が
        // 停滞と呼ばない**ことで解く (持ち仕事が無ければ出力が動かなくて
        // 当たり前なので、そもそも停滞ではない)。
        self.holding.is_none() && crate::coordinator::assignable(self.state)
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
    ///
    /// **一時的**。ほかのエージェントは居るが今は塞がっているだけなので、
    /// 誰かが終われば配れる。人へ上げると、勝手に解決したあとも消えない
    /// 判断が積み上がる。
    ReviewerWouldBeAuthor(TaskId),
    /// 実装担当**以外のエージェントが 1 体も居ない**。
    ///
    /// [`Unassigned::ReviewerWouldBeAuthor`] と違い、**待っても解決しない**
    /// (自分のレビューは禁止なので、この Run のままでは永久に配れない)。
    /// 人が並列数を増やすか、レビュー無しにするしかない。
    NoOtherReviewer(TaskId),
}

impl Unassigned {
    pub fn task(&self) -> TaskId {
        match self {
            Unassigned::NoCandidate(t)
            | Unassigned::CapsMissing { task: t, .. }
            | Unassigned::FileOverlap { task: t, .. }
            | Unassigned::ReviewerWouldBeAuthor(t)
            | Unassigned::NoOtherReviewer(t) => *t,
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
            Unassigned::NoOtherReviewer(t) => {
                format!("#{t}: レビューできるエージェントが実装担当のほかに居ません")
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
            //
            // **空集合に `all` を撃たない。** 「誰も空いていない」ときも
            // `all` は真を返すので、素直に書くと*ただ混んでいるだけ*の
            // レビューが「実装した本人しかレビュー候補がいません」になる。
            // 実機ではこれが 5 件積み上がり、待っても消えなかった
            // (混雑は次の tick で解ける — 人へ上げる話ではない)。
            let free_now: Vec<&Candidate> = candidates
                .iter()
                .filter(|c| c.free() && !taken.contains(&c.session))
                .collect();
            let only_author = author_session.is_some()
                && !free_now.is_empty()
                && free_now.iter().all(|c| Some(c.session) == author_session);
            // **待っても解決しない**のは、実装担当以外が 1 体も居ないとき
            // だけ (空いているかどうかではない)。
            let no_other = author_session.is_some()
                && !candidates
                    .iter()
                    .any(|c| Some(c.session) != author_session);
            out.unassigned.push(if no_other {
                Unassigned::NoOtherReviewer(t.id)
            } else if only_author {
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

    pub(super) fn cand(id: u64, state: SessionState, caps: &[&str]) -> Candidate {
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
        // **実装担当しか居ない → 待っても解決しない。**
        let only_author = vec![cand(1, SessionState::Idle, &[])];
        let p = plan_assignments(&tasks, &only_author, &BTreeMap::new());
        assert_eq!(p.unassigned, vec![Unassigned::NoOtherReviewer(2)]);
        // **他は居るが今は塞がっている → 一時的。**
        // 次の tick で空けば配れるので、`NoOtherReviewer` にしてはいけない。
        let mut busy = cand(2, SessionState::Working, &[]);
        busy.holding = Some(9);
        let one_free = vec![cand(1, SessionState::Idle, &[]), busy];
        let p3 = plan_assignments(&tasks, &one_free, &BTreeMap::new());
        assert_eq!(p3.unassigned, vec![Unassigned::ReviewerWouldBeAuthor(2)]);
        // **誰一人空いていない → ただ混んでいるだけ。**
        // 空集合へ `all` を撃つと真になるので、素直に書くとここが
        // 「実装した本人しか居ない」に化けて人の判断待ちが積み上がる。
        let mut a = cand(1, SessionState::Working, &[]);
        a.holding = Some(8);
        let mut b = cand(2, SessionState::Working, &[]);
        b.holding = Some(9);
        let p4 = plan_assignments(&tasks, &[a, b], &BTreeMap::new());
        assert_eq!(p4.unassigned, vec![Unassigned::NoCandidate(2)]);
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
    fn 配らないのは保有中と居ない相手と承認待ちだけ() {
        let tasks = vec![ready(1, "a", &["src/a.rs"])];
        let mut busy = cand(1, SessionState::Idle, &[]);
        busy.holding = Some(9);
        let cands = vec![
            busy,
            cand(2, SessionState::Exited, &[]),
            cand(3, SessionState::WaitingApproval, &[]),
        ];
        let p = plan_assignments(&tasks, &cands, &BTreeMap::new());
        assert!(p.assignments.is_empty(), "配ってはいけない相手へ配った");
        assert_eq!(p.unassigned, vec![Unassigned::NoCandidate(1)]);
    }

    /// **手ぶらの担当へは配れる。**
    ///
    /// 実測の止まり方: Planner / Tester / Reviewer が `stalled` で、`ready` の
    /// タスクが 5 本待っていた。空いている担当と待っている仕事が永久に
    /// 出会えなかった。
    ///
    /// **最初はここ (`free()`) を緩めて直したが、それは誤りだった。**
    /// 調停層は別の規則で断るので、**提案しては断られる**組み合わせが生まれ、
    /// 毎 tick 「割り当てを見送りました」が積まれて台帳 500 件がそれだけに
    /// なった。正しい直し場は `roles::derive_agent_work_state` — 仕事を
    /// 持っていない担当を**そもそも停滞と呼ばない** (`roles::tests::
    /// 仕事が無い担当は停滞ではない` が対の番人)。ここは配れることだけを見る。
    #[test]
    fn 手ぶらの担当へは配れる() {
        let tasks = vec![
            ready(1, "a", &[]),
            ready(2, "b", &[]),
            ready(3, "c", &[]),
            ready(4, "d", &[]),
        ];
        let cands = vec![
            cand(1, SessionState::Idle, &[]),
            cand(2, SessionState::Idle, &[]),
            cand(3, SessionState::AwaitingInput, &[]),
            cand(4, SessionState::Working, &[]),
        ];
        let p = plan_assignments(&tasks, &cands, &BTreeMap::new());
        assert_eq!(p.assignments.len(), 4, "手ぶらの担当へ配れていない");
        let mut got: Vec<_> = p.assignments.iter().map(|a| a.session).collect();
        got.sort();
        assert_eq!(got, vec![1, 2, 3, 4], "配り先が偏っている");
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

#[cfg(test)]
mod one_rule_tests {
    use super::tests::cand;
    use super::*;

    /// **配ってよいかの規則は 1 本だけ。**
    ///
    /// スケジューラと調停層が別々の規則を持つと、**提案しては断られる**
    /// 組み合わせが生まれ、毎 tick 「割り当てを見送りました」が積まれる。
    /// 実測で台帳 500 件がそれだけになり、計画も起動も伝言も押し出されて
    /// 人には何が起きたか一切追えなくなった。
    #[test]
    fn 配ってよいかの規則は調停層と同じ() {
        for st in [
            SessionState::Idle,
            SessionState::AwaitingInput,
            SessionState::Working,
            SessionState::WaitingApproval,
            SessionState::Stalled,
            SessionState::Exited,
            SessionState::Unknown,
        ] {
            let c = cand(1, st, &[]);
            assert_eq!(
                c.free(),
                crate::coordinator::assignable(st),
                "{st:?} でスケジューラと調停層の判断が違う (提案しては断られる)"
            );
        }
        // 保有中はどちらにせよ配らない。
        let mut busy = cand(1, SessionState::Idle, &[]);
        busy.holding = Some(9);
        assert!(!busy.free(), "保有中の担当へ配ろうとしている");
    }

    /// **規則をスケジューラ側に書き写していない。**
    /// 書き写すと、片方だけ直したときに静かにずれる。
    #[test]
    fn 規則を書き写していない() {
        let src = include_str!("scheduler.rs").replace("\r\n", "\n");
        let body = src
            .split("fn free(&self) -> bool {")
            .nth(1)
            .and_then(|t| t.split("\n    }\n").next())
            .expect("free がある");
        assert!(
            body.contains("coordinator::assignable"),
            "調停層の規則を借りていない"
        );
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("SessionState::"),
            "スケジューラ側に状態の一覧を書き写している"
        );
    }
}
