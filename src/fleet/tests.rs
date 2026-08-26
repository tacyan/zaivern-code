//! Phase 1 の番人。
//!
//! **どのテストも「直す前のコードでは赤になる」ことを確かめてから書いている。**
//! 空回りする番人を置かないための決まりで、経緯は CLAUDE.md
//! (「番人を足したら必ずわざと壊して赤になることを確かめる」) と同じ。
//!
//! 時刻は全部引数で注入する (`now_ms`)。実時間を待つと、負荷の高いマシンで
//! 閾値を跨いだ前後の振る舞いが観測できなくなる
//! (`terminal::scan_attention_at` / `lease::acquire_lock_in` と同じ作法)。

use super::model::{AgentKind, AgentKindOpt, Observation};
use super::store::FleetStore;
use crate::kanban::{Activity, Column, Flow, Source, TROUBLE_HOLD_MS};
use crate::supervisor::SessionState as S;

// ---------------------------------------------------------------------------
// 材料
// ---------------------------------------------------------------------------

/// PTY セッション 1 体の観測。
fn pty(id: u64, running: bool) -> Observation {
    Observation {
        id,
        kind: AgentKindOpt::pty(),
        title: format!("agent-{id}"),
        icon: "👾".into(),
        running,
        ..Default::default()
    }
}

/// 画面末尾つきの観測 (このティックで画面を読んだ、の意味)。
fn with_tail(mut o: Observation, lines: &[&str]) -> Observation {
    o.tail_lines = Some(lines.iter().map(|l| l.to_string()).collect());
    o
}

/// 構造化プロトコル段 (ラダー最上段) の判定を載せる。
fn with_ladder(mut o: Observation, state: crate::supervisor::protocol::ProtoState) -> Observation {
    o.ladder = Some(crate::supervisor::LadderRead {
        rung: crate::supervisor::Rung::Protocol,
        state,
        detail: String::new(),
    });
    o
}

// ---------------------------------------------------------------------------
// A. PC / スマホの状態一致
// ---------------------------------------------------------------------------

/// **同じ 1 体について、看板とスマホが同じレーン・同じラベルを返す。**
///
/// 直す前は看板が `classify_stream` (ラダー + 画面末尾 + flow + ヒステリシス)、
/// スマホが `column_for` (`ladder = None` / `tail = &[]` / `flow = Unknown`) を
/// 呼んでいたので、**構造化プロトコルが「編集中 ◆」と言っていても
/// スマホだけ「思考中 ≈」**になっていた。
///
/// いまはどちらも同じ [`super::Snapshot`] を読むので、
/// 「同じ値かどうか」ではなく **「同じ 1 個の値か」** を見れば足りる。
#[test]
fn pcとスマホは同じ判定を読む() {
    let mut fleet = FleetStore::default();
    // 構造化プロトコルが「編集中」と言っていて、画面末尾は無関係な文字列。
    let o = with_tail(
        with_ladder(
            pty(1, true),
            crate::supervisor::protocol::ProtoState::Editing,
        ),
        &["thinking about the problem"],
    );
    fleet.update(&[o], 0);

    let snap = fleet.snapshot();
    let v = snap.view(1).expect("ビューがある");

    // 看板が読む値
    let board_lane = v.lane;
    let board_label = v.state_label();
    // スマホ (`remote_reply_agents`) が読む値 — 同じ 1 本
    let mobile_lane = snap.view(1).map(|x| x.lane).unwrap();
    let mobile_label = snap.view(1).map(|x| x.state_label()).unwrap();

    assert_eq!(board_lane, mobile_lane);
    assert_eq!(board_label, mobile_label);

    // そして**その値がラダー由来である**こと (画面末尾へ落ちていない)。
    // ここが崩れると「同じ値だが両方とも間違っている」という緑になる。
    assert_eq!(
        v.activity,
        Activity::Editing,
        "構造化プロトコルを採っていない"
    );
    assert_eq!(v.source, Source::Protocol, "画面推定へ降りている");
    assert_eq!(v.lane, Column::Editing);
}

/// **弱い入口が復活していないことを、判定そのものの差で示す。**
///
/// ラダーを渡さない判定は「思考中」にしかならない。Store は必ず渡すので
/// 「編集中」になる。**この 2 つが同じ値になったら、Store がラダーを
/// 落としている**ということなので、番人として意味を持つ。
#[test]
fn storeはラダーを落とさない() {
    let mut fleet = FleetStore::default();
    let o = with_tail(
        with_ladder(
            pty(1, true),
            crate::supervisor::protocol::ProtoState::Editing,
        ),
        &[],
    );
    fleet.update(&[o.clone()], 0);
    let with = fleet.snap().view(1).unwrap().activity;

    // ラダーを外した同じ観測 (= 直す前のスマホが見ていた材料)
    let mut without = o;
    without.ladder = None;
    let mut bare = FleetStore::default();
    bare.update(&[without], 0);
    let got = bare.snap().view(1).unwrap().activity;

    assert_eq!(with, Activity::Editing);
    assert_ne!(
        with, got,
        "ラダーの有無で判定が変わらない = Store がラダーを見ていない"
    );
}

// ---------------------------------------------------------------------------
// B. 看板を閉じても状態が進む
// ---------------------------------------------------------------------------

/// **看板を 1 フレームも描かずに、時間依存の状態が進む。**
///
/// 直す前は `KanbanState::update_tracks` が `kanban::draw` からしか呼ばれず、
/// `kanban_ui` は `center == CenterView::Kanban` のフレームでしか走らなかった。
/// つまりこのテストの状況 (看板を開いていない) では **`TROUBLE_HOLD_MS` の
/// 計時が 1 ミリ秒も進まず**、永遠に `Trouble` へ落ちなかった。
///
/// ここでは `FleetStore` だけを回す — egui の `Ui` も `Context` も出てこない。
#[test]
fn 看板を描かなくても停滞判定は進む() {
    let mut fleet = FleetStore::default();
    // 見張りが「停滞」と言い続け、画面は 1 行も増えない (= flow は Silent へ)。
    let frozen = |t: u64| {
        let mut o = with_tail(pty(1, true), &["⠋ Thinking… (12s)"]);
        o.sup = Some(S::Stalled);
        let _ = t;
        o
    };

    // まず作業中として観測する (生まれた瞬間はその場に置かれるので、
    // ヒステリシスが掛かるのは**移動**のときだけ)。
    let mut working = with_tail(pty(1, true), &["⏺ Bash(cargo test)"]);
    working.sup = Some(S::Working);
    fleet.update(&[working], 0);
    assert_eq!(fleet.snap().view(1).unwrap().lane, Column::Verifying);

    // ここから停滞。1 サンプルでは人を呼ばない (継続確認が要る)。
    fleet.update(&[frozen(1_000)], 1_000);
    assert_ne!(
        fleet.snap().view(1).unwrap().lane,
        Column::Trouble,
        "1 サンプルで人を呼んでいる"
    );

    // 画面を読む刻みで時間だけ進める。**看板は 1 度も描かない。**
    let mut t = 1_000u64;
    for _ in 0..40 {
        t += 1_000;
        fleet.update(&[frozen(t)], t);
    }

    let v = fleet.snap().view(1).unwrap();
    assert!(
        t > TROUBLE_HOLD_MS,
        "テストが継続確認の閾値を跨いでいない (t={t})"
    );
    assert_eq!(
        v.flow,
        Flow::Silent,
        "出力が止まっている裏取りが取れていない"
    );
    assert_eq!(
        v.lane,
        Column::Trouble,
        "看板を閉じている間に停滞判定が進んでいない"
    );
}

// ---------------------------------------------------------------------------
// C. 画面を切り替えても追跡がリセットされない
// ---------------------------------------------------------------------------

/// **看板 → デッキ → Cockpit → 看板 と切り替えても `Track` が生き続ける。**
///
/// 直す前は看板とデッキが**別々の** `tracks` を持ち、しかもどちらも
/// 自分が描かれているフレームでしか進まなかったので、切り替えるたびに
/// `Track::new` からやり直し = ヒステリシスも継続確認もリセットされていた。
///
/// `FleetStore` は `ZaivernApp` が 1 つだけ持ち、`fleet_tick` が
/// **どのフレームでも**回すので、そもそも「いま何の画面か」を知らない。
/// ここでは追跡の連続性を、**遷移が起きた時刻**で確かめる:
/// 継続確認は「最初に停滞と読めた時刻 + `TROUBLE_HOLD_MS`」ちょうどで
/// 満ちるはずで、途中で作り直されていればもっと後ろへずれる。
#[test]
fn 画面を切り替えても追跡は続く() {
    let mut fleet = FleetStore::default();
    let stalled = || {
        let mut o = with_tail(pty(1, true), &["⠋ Thinking… (12s)"]);
        o.sup = Some(S::Stalled);
        o
    };

    // 生まれた瞬間はその場に置かれるので、まず作業中として観測する。
    let mut working = with_tail(pty(1, true), &["⏺ Bash(cargo test)"]);
    working.sup = Some(S::Working);
    fleet.update(&[working], 0);

    // 以降ずっと停滞。1 秒刻みで回す。
    // 「看板を見ている」→「デッキ/Cockpit を見ている」→「看板へ戻った」を
    // またいでも、Store から見れば同じ 1 本の連続した観測でしかない。
    let mut first_trouble: Option<u64> = None;
    let mut first_silent: Option<u64> = None;
    for t in (1_000..=40_000).step_by(1_000) {
        fleet.update(&[stalled()], t);
        let v = fleet.snap().view(1).unwrap();
        if first_silent.is_none() && v.flow == Flow::Silent {
            first_silent = Some(t);
        }
        if first_trouble.is_none() && v.lane == Column::Trouble {
            first_trouble = Some(t);
        }
    }

    let silent = first_silent.expect("出力が止まっている裏取りが取れていない");
    let trouble = first_trouble.expect("停滞判定がいつまでも人を呼ばない");
    assert_eq!(
        trouble,
        silent + TROUBLE_HOLD_MS,
        "継続確認が途中でやり直しになっている (silent={silent} trouble={trouble})"
    );

    // 追跡が作り直されていれば `since_ms` は最後のティックになる。
    let v = fleet.snap().view(1).unwrap();
    assert!(
        v.since_ms < 40_000,
        "追跡が作り直されている (since_ms={})",
        v.since_ms
    );
}

/// 消えたセッションの追跡は捨てる (無限に太らせない)。
#[test]
fn 消えたセッションの追跡は捨てる() {
    let mut fleet = FleetStore::default();
    fleet.update(&[pty(1, true), pty(2, true)], 0);
    assert_eq!(fleet.tracked(), 2);
    fleet.update(&[pty(1, true)], 1_000);
    assert_eq!(fleet.tracked(), 1);
    fleet.update(&[], 2_000);
    assert_eq!(fleet.tracked(), 0);
}

// ---------------------------------------------------------------------------
// D. スマホ JSON の契約
// ---------------------------------------------------------------------------

/// **レーン番号は `Column::index()` のまま** — スマホ JS はこの数字で
/// 見出しと本文を突き合わせている。ここがずれると、対応表を持っている
/// `assets/remote/js/70-boards.js` が黙って別のレーンへ振り分ける。
#[test]
fn レーン番号の対応は変わっていない() {
    // 直す前の `/api/agents` が返していた並びと同じであること。
    let want = [
        (Column::Ready, 0),
        (Column::Thinking, 1),
        (Column::Editing, 2),
        (Column::Running, 3),
        (Column::Verifying, 4),
        (Column::Approval, 5),
        (Column::Trouble, 6),
        (Column::Done, 7),
    ];
    for (col, i) in want {
        assert_eq!(col.index(), i, "{col:?} のレーン番号が変わった");
    }
    assert_eq!(crate::kanban::LANES, 8);
}

/// **`waiting` の判定は 1 か所 (`remote::is_waiting_lane`)。**
///
/// スマホは「待ち」ビューのバッジ (`/api/state`) と一覧 (`/api/agents`) で
/// 同じ数を出す。数え方が 2 つあると「バッジ 3 なのに一覧は 5 件」になる。
#[test]
fn 待ちレーンの判定は看板と同じ() {
    for col in crate::kanban::COLUMNS {
        let waiting = crate::remote::is_waiting_lane(col);
        // 「人の手が要る」レーン (`Column::loud`) は必ず待ち。
        if col.loud() {
            assert!(waiting, "{col:?} が待ちに数えられていない");
        }
    }
}

// ---------------------------------------------------------------------------
// E. 集計の不変条件
// ---------------------------------------------------------------------------

/// **レーン別人数の合計 == 総数。** 二重計上も取りこぼしも無い。
#[test]
fn レーンの合計は総数と一致する() {
    let mut fleet = FleetStore::default();
    let mut obs = Vec::new();
    for id in 1..=7u64 {
        let mut o = with_tail(pty(id, id % 3 != 0), &["⏺ Bash(cargo test)"]);
        o.sup = Some(if id % 2 == 0 { S::Working } else { S::Idle });
        obs.push(o);
    }
    fleet.update(&obs, 0);

    let snap = fleet.snapshot();
    let all = snap.tally(None);
    assert_eq!(all.total, 7);
    assert_eq!(all.lane_sum(), all.total, "レーン集計が総数と合わない");
    // `running` はレーンではなくプロセスの生死 (別軸なので合計に混ぜない)。
    assert_eq!(all.running, 5);
}

// ---------------------------------------------------------------------------
// F. ACP も Fleet に載る
// ---------------------------------------------------------------------------

/// **ACP セッションも必ず 1 本のレーンに入り、総数に含まれる。**
///
/// 直す前は `acp::AcpManager` が `kanban` / `deck` / スマホのどこからも
/// 参照されておらず (`kanban.rs` に `acp` の文字列は 1 件も無かった)、
/// ラダーの**最上段**で駆動しているエージェントが Fleet の総数から
/// 丸ごと漏れていた。
#[test]
fn acpも1本のレーンに入り総数に含まれる() {
    let mut fleet = FleetStore::default();
    let acp = |id: u64, phase: crate::acp::Phase| {
        let mut o = pty(id, true);
        o.kind = AgentKindOpt::acp();
        o.ladder = Some(crate::supervisor::LadderRead {
            rung: crate::supervisor::Rung::Protocol,
            state: crate::acp::proto_state_of(&phase),
            detail: phase.label(),
        });
        o.tail_lines = Some(Vec::new());
        // 実装 (`acp::AcpManager::fleet_observations`) と同じ写し方。
        // `Failed` を「終わった」にすると完了レーンへ入ってしまう。
        o.running = !matches!(phase, crate::acp::Phase::Ended);
        o
    };
    let obs = vec![
        with_tail(pty(1, true), &["⏺ Bash(cargo test)"]),
        acp(1 << 48, crate::acp::Phase::Running),
        acp((1 << 48) + 1, crate::acp::Phase::Idle),
        acp((1 << 48) + 2, crate::acp::Phase::Failed("boom".into())),
    ];
    fleet.update(&obs, 0);
    let snap = fleet.snapshot();

    // 総数は 4 (ACP 3 + PTY 1)。レーンの合計と必ず一致する。
    let all = snap.tally(None);
    assert_eq!(all.total, 4, "ACP が総数へ入っていない");
    assert_eq!(all.lane_sum(), all.total);

    // レーンに並ぶカードは PTY だけなので、タイルは Pty で数える
    // (タイルの数字と並んでいるカード数を必ず一致させる)。
    let pty_only = snap.tally(Some(AgentKind::Pty));
    assert_eq!(pty_only.total, 1);
    assert_eq!(pty_only.lane_sum(), pty_only.total);

    // ACP の 1 体ずつが、ちょうど 1 本のレーンを持っている。
    for id in [1u64 << 48, (1 << 48) + 1, (1 << 48) + 2] {
        let v = snap.view(id).unwrap_or_else(|| panic!("{id} が居ない"));
        assert_eq!(v.kind, AgentKind::Acp);
        assert!(crate::kanban::COLUMNS.contains(&v.lane));
        // **画面推定へ降りていない** (ACP は最上段で駆動している)。
        assert_eq!(v.source, Source::Protocol, "{id} が画面推定へ降りた");
    }

    // **失敗は「完了」ではない。** 見なくてよい側へ落とすと事故になる。
    let failed = snap.view((1 << 48) + 2).unwrap();
    assert_ne!(failed.lane, Column::Done, "ACP の失敗が完了レーンへ入った");
    assert_eq!(failed.activity, Activity::Stalled);
}

/// **ACP を足してもスマホ向けの集計が 1 つも変わらない。**
///
/// レビュー指摘の回帰テスト。`lane_counts` / `stuck_ids` / `waiting_count` が
/// `snap.agents` を全件数えていたため、スマホの一覧 (PTY のみ) と
/// 見出しの件数・待ちバッジが食い違いうる状態だった。
///
/// **数える対象と並べる対象は必ず同じでなければならない。**
#[test]
fn acpを足してもスマホ向け集計は変わらない() {
    use super::projection::{lane_counts, stuck_ids, waiting_count};
    let pty_only = || {
        vec![
            {
                // 検証中 = 「待ち」に数えられない 1 体。
                // **見張りの判定を渡さないと `Starting` → `Ready` になり、
                // `is_waiting_lane` は `Ready` も待ちに数える**ので、
                // ここを空にすると「待ち 2」になってテストの主張がぼやける。
                let mut o = with_tail(pty(1, true), &["⏺ Bash(cargo test)"]);
                o.sup = Some(S::Working);
                o
            },
            {
                // 承認待ち = 「待ち」に数えられる 1 体
                let mut o = with_tail(pty(2, true), &["Do you want to proceed?"]);
                o.attention = true;
                o
            },
        ]
    };
    let acp = |id: u64, phase: crate::acp::Phase| {
        let mut o = pty(id, true);
        o.kind = AgentKindOpt::acp();
        o.ladder = Some(crate::supervisor::LadderRead {
            rung: crate::supervisor::Rung::Protocol,
            state: crate::acp::proto_state_of(&phase),
            detail: phase.label(),
        });
        o.tail_lines = Some(Vec::new());
        o.running = !matches!(phase, crate::acp::Phase::Ended);
        o
    };

    // ── PTY 2 体だけ ──
    let mut before = FleetStore::default();
    before.update(&pty_only(), 0);
    let b = before.snapshot();
    let kind = Some(AgentKind::Pty);
    let (b_counts, b_wait, b_stuck) = (
        lane_counts(&b, kind),
        waiting_count(&b, kind),
        stuck_ids(&b, kind),
    );

    // ── ACP を 2 体足す (うち 1 体は Failed = 人を呼ぶレーン) ──
    let mut after = FleetStore::default();
    let mut obs = pty_only();
    obs.push(acp(1 << 48, crate::acp::Phase::Running));
    obs.push(acp((1 << 48) + 1, crate::acp::Phase::Failed("boom".into())));
    after.update(&obs, 0);
    let a = after.snapshot();
    let (a_counts, a_wait, a_stuck) = (
        lane_counts(&a, kind),
        waiting_count(&a, kind),
        stuck_ids(&a, kind),
    );

    // **スマホ向けの数字が 1 つも動かない。**
    assert_eq!(
        a_counts, b_counts,
        "ACP を足してレーン見出しの件数が変わった"
    );
    assert_eq!(a_wait, b_wait, "ACP を足して待ちバッジが変わった");
    assert_eq!(a_stuck, b_stuck, "ACP を足して停滞一覧が変わった");

    // 依頼の必須条件を明示的に固定する。
    assert_eq!(a_counts.iter().sum::<usize>(), 2, "見出しの合計が 2 でない");
    assert_eq!(a.tally(kind).total, 2, "スマホ向けの総数が 2 でない");
    assert_eq!(a_wait, 1, "待ちは承認待ちの 1 体だけのはず");
    // 一覧に並ぶのは PTY セッションだけ (スマホの操作 API は index を宛先に使う)
    let listed: Vec<u64> = a
        .agents
        .iter()
        .filter(|v| v.kind == AgentKind::Pty)
        .map(|v| v.id)
        .collect();
    assert_eq!(listed, vec![1, 2], "一覧の件数が 2 でない");

    // **Fleet 全体は 4 体**。総数の顔と、スマホの一覧の顔を混ぜない。
    assert_eq!(
        a.tally(None).total,
        4,
        "Fleet 全体の総数に ACP が入っていない"
    );
    assert_eq!(a.tally(None).lane_sum(), 4);
    // 全体で数えれば ACP の Failed が「待ち」へ入る (絞り込みの違いが効いている証拠)
    assert!(
        waiting_count(&a, None) > a_wait,
        "絞り込みが効いていない (全体と PTY-only が同じ数)"
    );
}

/// ACP の接続段 → 構造化状態の写像 (表で固定する)。
#[test]
fn acpの段の写像は表のとおり() {
    use crate::acp::Phase;
    use crate::supervisor::protocol::ProtoState as P;
    let cases = [
        (Phase::Initializing, P::Starting),
        (Phase::CreatingSession, P::Starting),
        (Phase::Idle, P::Idle),
        (Phase::Running, P::Thinking),
        (Phase::Failed("x".into()), P::Failed),
        (Phase::Ended, P::Done),
    ];
    for (phase, want) in cases {
        assert_eq!(crate::acp::proto_state_of(&phase), want, "{phase:?}");
    }
}

// ---------------------------------------------------------------------------
// 費用 (絶対時間で線を引かない — 守りたい性質そのものを測る)
// ---------------------------------------------------------------------------

/// **1 ティックの費用がエージェント数 N に対して O(N) であること。**
///
/// 直す前のこの番人は「N 体入れたら N 個のビューが返る」しか見ておらず、
/// **`step_tracks` が O(N²) のままでも緑だった** (レビューで指摘された)。
/// 返り値の個数は計算量と何の関係も無いので、あれは計算量の番人ではなく
/// 「取りこぼしが無い」の番人でしかなかった。
///
/// CLAUDE.md の「絶対時間で性能テストの線を引かない。必ず嘘をつく」に従い、
/// 秒ではなく**件数を 2 倍にしたときの伸び**を見る。数えるのは追跡表の掃除で
/// 見た候補の数 ([`super::engine::take_prune_probes`]) で、
/// これは O(N) 実装ならちょうど 2N、追跡ごとに観測列を舐め直す O(N²) 実装なら
/// N² 規模になる。
#[test]
fn 一ティックの費用はエージェント数に線形() {
    // 生きている N 体を積んだ Store を作り、次のティックの掃除の費用を数える。
    let probes = |n: u64| -> usize {
        let mut fleet = FleetStore::default();
        let obs: Vec<Observation> = (1..=n)
            .map(|id| with_tail(pty(id, true), &["⏺ Bash(cargo test)"]))
            .collect();
        fleet.update(&obs, 0);
        // ここまでの探針は捨てる (測りたいのは「追跡が N 本ある状態の 1 ティック」)。
        let _ = super::engine::take_prune_probes();
        fleet.update(&obs, 1_000);
        super::engine::take_prune_probes()
    };

    let a = probes(50);
    let b = probes(100);

    // O(N): 生きている ID の集合を 1 回作る (N) + 追跡 1 本につき 1 回引く (N)。
    assert_eq!(a, 100, "50 体の掃除が 2N になっていない");
    assert_eq!(b, 200, "100 体の掃除が 2N になっていない");
    // **伸びがちょうど 2 倍**。O(N²) ならここが 4 倍側へ跳ねる。
    assert_eq!(b, a * 2, "件数を 2 倍にしたら費用も 2 倍 (線形) のはず");
}

/// 取りこぼしが無いこと (旧「線形」テストが実際に見ていた性質)。
/// 計算量とは別の話なので、名前も別にして残す。
#[test]
fn 観測した体数だけビューが返る() {
    let run = |n: u64| -> usize {
        let mut fleet = FleetStore::default();
        let obs: Vec<Observation> = (1..=n)
            .map(|id| with_tail(pty(id, true), &["⏺ Bash(cargo test)"]))
            .collect();
        fleet.update(&obs, 0);
        fleet.snap().agents.len()
    };
    assert_eq!(run(50), 50);
    assert_eq!(run(100), 100);
}

/// **画面を読まないティックでも判定は落ちない** (前回サンプルを使い回す)。
///
/// `sample_due` が false のティックは `tail_lines: None` で渡ってくる。
/// ここで空の画面として扱うと、判定が毎回「思考中」へ落ちて往復する。
#[test]
fn 画面を読まないティックは前回を使い回す() {
    let mut fleet = FleetStore::default();
    let mut o = with_tail(pty(1, true), &["⏺ Bash(cargo test)"]);
    o.sup = Some(S::Working);
    fleet.update(&[o.clone()], 0);
    assert_eq!(fleet.snap().view(1).unwrap().activity, Activity::Verifying);

    // 画面を読まなかったティック
    let mut stale = pty(1, true);
    stale.sup = Some(S::Working);
    stale.tail_lines = None;
    fleet.update(&[stale], 100);
    assert_eq!(
        fleet.snap().view(1).unwrap().activity,
        Activity::Verifying,
        "画面を読まないティックで判定が落ちた"
    );
}

/// 画面の読み直しは間引く (動いている間 ~6.7Hz / 静かなら 1Hz)。
/// 看板が持っていた間引きをそのまま移したので、**費用は増えていない**。
#[test]
fn 画面の読み直しは間引かれる() {
    let mut fleet = FleetStore::default();
    assert!(fleet.sample_due(0), "初回は必ず読む");
    assert!(!fleet.sample_due(500));
    assert!(fleet.sample_due(1_000), "静かなときは 1Hz");

    // 動き出したら速くなる
    let mut o = with_tail(pty(1, true), &["⏺ Bash(cargo build)"]);
    o.sup = Some(S::Working);
    fleet.update(&[o], 1_000);
    assert!(!fleet.sample_due(1_100));
    assert!(fleet.sample_due(1_200), "動いているときは ~6.7Hz");
}

// ---------------------------------------------------------------------------
// 初回ティックの網羅性 (リモート応答が fallback へ落ちないこと)
// ---------------------------------------------------------------------------

/// **1 回目の更新で、観測した全員がスナップショットに載る。**
///
/// `remote_reply_agents` は `snap.view(s.id)` が `None` のとき
/// `Column::Ready` / `Activity::Starting` へ落ちる。この fallback は
/// 「Store がまだそのセッションを知らない」ときにしか使われてはいけない —
/// 起動直後の 1 フレームで使われると、**一覧は N 件なのにレーン見出しは 0**
/// という不整合になる (レビュー指摘)。
///
/// ここは「Store 側は 1 ティックで必ず全員を載せる」を固定する。
/// 「リモートがそのティックの後に読む」ほうは
/// `app::deck_wiring_tests::fleetを読むリモート応答は更新後に作る` が見る。
#[test]
fn 初回ティックで全エージェントがスナップショットに載る() {
    let mut fleet = FleetStore::default();
    // 復元直後を模す: 見張りの判定も画面もまだ何も無い 2 体。
    let obs = vec![pty(1, true), pty(2, true)];
    fleet.update(&obs, 0);

    let snap = fleet.snapshot();
    let kind = Some(AgentKind::Pty);

    // 依頼の必須条件 1: 各エージェントが必ず居る (= fallback へ落ちない)
    for o in &obs {
        assert!(
            snap.view(o.id).is_some(),
            "id={} が初回スナップショットに居ない (fallback に落ちる)",
            o.id
        );
    }
    // 依頼の必須条件 2: agents.length == sum(counts)
    let counts = super::projection::lane_counts(&snap, kind);
    assert_eq!(
        counts.iter().sum::<usize>(),
        obs.len(),
        "一覧の件数とレーン見出しの合計が食い違っている"
    );
    assert_eq!(snap.tally(kind).total, obs.len());
    assert_eq!(snap.tally(kind).lane_sum(), obs.len());
}

/// 途中でエージェントが増えても、その**同じティック**で載る
/// (次のティックまで一覧と見出しがずれない)。
#[test]
fn 増えたエージェントも同じティックで載る() {
    let mut fleet = FleetStore::default();
    fleet.update(&[pty(1, true)], 0);
    assert_eq!(fleet.snap().agents.len(), 1);

    fleet.update(&[pty(1, true), pty(2, true), pty(3, true)], 1_000);
    let snap = fleet.snapshot();
    for id in [1u64, 2, 3] {
        assert!(snap.view(id).is_some(), "id={id} が載っていない");
    }
    let counts = super::projection::lane_counts(&snap, Some(AgentKind::Pty));
    assert_eq!(counts.iter().sum::<usize>(), 3);
}
