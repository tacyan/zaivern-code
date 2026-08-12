use super::*;
use crate::coordinator::SessionState as C;
use crate::supervisor::SessionState as S;

fn intent(
    action: supervisor::Intervention,
    needs_confirmation: bool,
) -> supervisor::InterventionIntent {
    supervisor::InterventionIntent {
        session_id: 1,
        session_title: "テスト".into(),
        action,
        anomaly: supervisor::Anomaly::Stall,
        reason: "理由".into(),
        needs_confirmation,
        payload: None,
        at_ms: 0,
    }
}

// ── セッション状態マッピング ──────────────────────────────

#[test]
fn dead_process_is_exited() {
    // 生きていなければ、監視の見立てが何であろうと終了。
    for sup in [None, Some(S::Working), Some(S::Idle), Some(S::Done)] {
        assert_eq!(coordinator_state(false, false, false, sup), C::Exited);
        assert_eq!(coordinator_state(false, true, false, sup), C::Exited);
    }
}

#[test]
fn attention_wins_over_everything_while_running() {
    // 承認プロンプトで止まっている相手には割り込ませない。
    for sup in [None, Some(S::Working), Some(S::Idle)] {
        assert_eq!(
            coordinator_state(true, true, false, sup),
            C::WaitingApproval
        );
    }
}

#[test]
fn recent_output_is_working_and_quiet_prompt_is_idle() {
    assert_eq!(
        coordinator_state(true, false, false, Some(S::Working)),
        C::Working
    );
    assert_eq!(
        coordinator_state(true, false, false, Some(S::Idle)),
        C::Idle
    );
}

#[test]
fn unobserved_session_is_unknown_not_idle() {
    // まだ一度も観測していない = 何も分からない。ここを Idle にすると
    // 起動直後の忙しいエージェントへ文字を流し込んでしまう。
    assert_eq!(coordinator_state(true, false, false, None), C::Unknown);
}

#[test]
fn ambiguous_states_map_to_unknown() {
    // ループ / エラー多発 / 異常終了 / 完了扱いは「いま入力を受け付けられるか」
    // が判断できない。すべて Unknown に倒す。
    for sup in [S::Looping, S::Errored, S::Crashed, S::Done] {
        assert_eq!(
            coordinator_state(true, false, false, Some(sup)),
            C::Unknown,
            "{sup:?} は曖昧なので Unknown でなければならない"
        );
    }
}

#[test]
fn only_idle_is_deliverable_among_running_states() {
    // 配達されうるのは待機だけ、という不変条件を coordinator 側の判定で確かめる。
    let cases = [
        (None, false),
        (Some(S::Working), false),
        (Some(S::Idle), true),
        (Some(S::WaitingApproval), false),
        (Some(S::Stalled), false),
        (Some(S::Looping), false),
        (Some(S::Errored), false),
        (Some(S::Crashed), false),
        (Some(S::Done), false),
    ];
    for (sup, want) in cases {
        let st = coordinator_state(true, false, false, sup);
        assert_eq!(
            coordinator::deliverable(st),
            want,
            "sup={sup:?} → {st:?} の配達可否が想定と違う"
        );
    }
}

#[test]
fn stalled_session_is_never_delivered_to() {
    let st = coordinator_state(true, false, false, Some(S::Stalled));
    assert_eq!(st, C::Stalled);
    assert!(!coordinator::deliverable(st));
}

#[test]
fn rate_limited_session_is_stalled_for_assignment() {
    // レート制限中は進めない: 新規タスクを振らず、メッセージも配達しない
    let st = coordinator_state(true, false, true, Some(S::Idle));
    assert_eq!(st, C::Stalled);
    assert!(!coordinator::deliverable(st));
    // 承認待ちの方が優先 (制限中でも承認には応えられる)
    assert_eq!(
        coordinator_state(true, true, true, Some(S::Idle)),
        C::WaitingApproval
    );
    // 制限が解けたら通常判定に戻る
    assert_eq!(
        coordinator_state(true, false, false, Some(S::Idle)),
        C::Idle
    );
}

// ── 確認ゲート ──────────────────────────────────────────

#[test]
fn needs_confirmation_intent_never_runs_directly() {
    // 確認フラグが立っていたら、どの操作であろうと確認ダイアログ行き。
    for action in [
        supervisor::Intervention::Observe,
        supervisor::Intervention::Notify,
        supervisor::Intervention::AutoAnswer,
        supervisor::Intervention::Nudge,
        supervisor::Intervention::Restart,
        supervisor::Intervention::Halt,
    ] {
        assert_eq!(
            route_intent(&intent(action, true)),
            IntentRoute::Confirm,
            "{action:?} は確認が必要なのに無確認で実行されようとしている"
        );
    }
}

#[test]
fn destructive_intents_are_confirmed_even_without_the_flag() {
    // 二重の歯止め: 確認フラグが落ちていても再起動・停止は無確認で走らせない。
    for action in [
        supervisor::Intervention::Restart,
        supervisor::Intervention::Halt,
    ] {
        assert_eq!(route_intent(&intent(action, false)), IntentRoute::Confirm);
    }
}

#[test]
fn harmless_intents_run_without_a_dialog() {
    for action in [
        supervisor::Intervention::Observe,
        supervisor::Intervention::Notify,
        supervisor::Intervention::AutoAnswer,
        supervisor::Intervention::Nudge,
    ] {
        assert_eq!(route_intent(&intent(action, false)), IntentRoute::Run);
    }
}

#[test]
fn commander_notice_goes_to_user_and_drops_unknown_targets() {
    let titles = vec!["Claude — main".to_string(), "codex-1".to_string()];
    let ds = crate::commander::parse_directives(
        "@all: 全員テストを回して\n@codex: ビルドを直して\n@居ない人: これは誤爆",
        crate::coordinator::INJECT_PREFIX,
    );
    assert_eq!(ds.len(), 3);
    // 全員宛・実在する宛先はユーザー向けの通知文になる
    let all = commander_notice(&ds[0], &titles).expect("全員宛は通知になるはず");
    assert!(all.contains("全員テストを回して"));
    let named = commander_notice(&ds[1], &titles).expect("実在宛先は通知になるはず");
    assert!(named.contains("codex-1"));
    assert!(named.contains("ビルドを直して"));
    // 実在しない宛先の誤爆は黙って捨てる
    assert!(commander_notice(&ds[2], &titles).is_none());
}

#[test]
fn auto_yes_requires_explicit_opt_in() {
    // 自動YESは設定 `pet_auto_yes` だけで決まり、既定は OFF。ペットや
    // バブルの表示状態には依存しない (勝手にYESが送られる報告への恒久対策)。
    // update() の `allow_auto_yes = self.cfg.pet_auto_yes` がこの前提に立つ。
    let cfg = crate::config::Config::default();
    assert!(!cfg.pet_auto_yes, "既定はユーザー承認必須 (自動YESしない)");
}

#[test]
fn supervisor_gate_and_app_route_agree_on_restart_and_halt() {
    // 既定設定では再起動・停止はどの承認モードでも「要確認」。
    // その結果を app 側のルーティングが必ず確認へ回すことまで通しで確かめる。
    let cfg = supervisor::SupervisorConfig::default();
    for (mode, approval) in [
        ("ask", crate::agents::Approval::Ask),
        ("auto", crate::agents::Approval::Auto),
        ("agent", crate::agents::Approval::Agent),
    ] {
        for action in [
            supervisor::Intervention::Restart,
            supervisor::Intervention::Halt,
        ] {
            let g = supervisor::gate(action, approval, &cfg);
            assert!(
                matches!(g, supervisor::GateResult::NeedConfirm(_)),
                "{action:?} / {mode} が既定で自動実行になっている: {g:?}"
            );
            assert_eq!(route_intent(&intent(action, true)), IntentRoute::Confirm);
        }
    }
}
