use super::*;

/// 画面が見えていてフォーカスもあるが、何も起きていない既定形。
fn idle() -> IdleSignals {
    IdleSignals {
        focused: true,
        visible: true,
        ..Default::default()
    }
}

/// 回帰テスト: 完全なアイドル (入力なし / アニメなし / エージェントなし /
/// 家事なし) では**1 フレームも予約しない**。ここが Some に戻ったら
/// アイドル時の CPU が跳ねる。
#[test]
fn fully_idle_asks_for_nothing() {
    assert_eq!(idle_repaint_ms(idle()), None);
}

/// 入力があっても、追加の予約はしない (egui が入力で起こす)。
#[test]
fn input_alone_does_not_schedule() {
    let s = IdleSignals {
        had_input: true,
        ..idle()
    };
    assert_eq!(idle_repaint_ms(s), None);
}

/// アニメーションが飛んでいるときは持ち主に任せる。
#[test]
fn animation_is_left_to_its_owner() {
    let s = IdleSignals {
        animating: true,
        ..idle()
    };
    assert_eq!(idle_repaint_ms(s), None);
    // 家事があってもアニメの刻みの方が細かいので任せる
    let s = IdleSignals {
        animating: true,
        watching_files: true,
        ..idle()
    };
    assert_eq!(idle_repaint_ms(s), None);
}

/// 応答待ちはアニメより優先。取りこぼすと選んだファイルが開かない。
#[test]
fn awaiting_wins_over_animation() {
    let s = IdleSignals {
        awaiting: true,
        animating: true,
        ..idle()
    };
    assert_eq!(idle_repaint_ms(s), Some(IDLE_AWAITING_MS));
}

/// エージェントが走っている間は、出力が無くても状態機械を進める。
#[test]
fn running_agents_keep_ticking() {
    let s = IdleSignals {
        agents_running: true,
        ..idle()
    };
    assert_eq!(idle_repaint_ms(s), Some(IDLE_AGENT_MS));
    // 背面ではもっと緩める (通知は別スレッドから届く)
    let s = IdleSignals {
        focused: false,
        ..s
    };
    assert_eq!(idle_repaint_ms(s), Some(IDLE_AGENT_BACKGROUND_MS));
    // フォーカス情報が取れない環境でも、直前に入力があれば前景扱い
    let s = IdleSignals {
        had_input: true,
        ..s
    };
    assert_eq!(idle_repaint_ms(s), Some(IDLE_AGENT_MS));
}

/// 家事 (外部変更の取り込み) があるときだけ、低頻度で回す。
#[test]
fn housekeeping_backs_off_with_visibility() {
    let s = IdleSignals {
        watching_files: true,
        ..idle()
    };
    assert_eq!(idle_repaint_ms(s), Some(IDLE_HOUSEKEEP_MS));
    let s = IdleSignals {
        focused: false,
        ..s
    };
    assert_eq!(idle_repaint_ms(s), Some(IDLE_BACKGROUND_MS));
    let s = IdleSignals {
        visible: false,
        ..s
    };
    assert_eq!(idle_repaint_ms(s), Some(IDLE_HIDDEN_MS));
    // タイマ (自動保存・interval プラグイン) だけでも同じ扱い
    let s = IdleSignals {
        timers_due: true,
        ..idle()
    };
    assert_eq!(idle_repaint_ms(s), Some(IDLE_HOUSEKEEP_MS));
}

/// 実際に `egui::Context` を回して「予約されたか」を見る。
///
/// `idle_repaint_ms` の単体テストは**判断**しか見ていないので、
/// 予約の呼び出しを足した/落とした事故は素通りする。ここでは
/// [`ZaivernApp::schedule_idle_repaint`] と同じ形でフレームを回し、
/// egui が実際に返す `repaint_delay` を確かめる。
///
/// `Duration::MAX` = 「次のフレームを予約していない」= 完全に寝る。
fn drive(signals: IdleSignals, frames: usize) -> Duration {
    let ctx = egui::Context::default();
    let mut delay = Duration::ZERO;
    for _ in 0..frames {
        let out = ctx.run(egui::RawInput::default(), |ctx| {
            // 何か 1 つは描く (完全に空だと egui が別の経路を通る)
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.label("idle");
            });
            // schedule_idle_repaint と同じ判断・同じ予約
            let s = IdleSignals {
                animating: ctx.has_requested_repaint(),
                ..signals
            };
            if let Some(ms) = idle_repaint_ms(s) {
                crate::perf::repaint_after(ctx, Duration::from_millis(ms), "drive");
            }
        });
        delay = out
            .viewport_output
            .values()
            .map(|v| v.repaint_delay)
            .min()
            .expect("ルートビューポートが必ずある");
    }
    delay
}

/// **本命の回帰テスト**: 何も起きていないとき、egui へ 1 フレームも
/// 予約しない。ここが `Duration::MAX` 以外に戻ったら、アイドルで
/// CPU を焼き始めたということ (設計原則 3 の破れ)。
#[test]
fn ヘッドレスのアイドルでは再描画が要求されない() {
    // 1 フレーム目はレイアウト確定で egui 自身が要求しうるので数フレーム回す
    let delay = drive(idle(), 4);
    assert_eq!(
        delay,
        Duration::MAX,
        "アイドルなのに {delay:?} 後の再描画が予約されている"
    );
}

/// 予約が「だいたい要求どおり」であることを見る。
///
/// egui 0.29 は `request_repaint_after` で受けた遅延から
/// **予測フレーム時間 (`predicted_dt`、既定 1/60 秒) を引く**
/// (`context.rs` の `delay.saturating_sub(predicted_frame_time)`。
/// 目標を行き過ぎないための調整)。だから要求値ちょうどは返らない。
/// ここではその 1 フレームぶんだけを許容し、桁が変わる事故は落とす。
fn assert_scheduled(got: Duration, want_ms: u64) {
    let want = Duration::from_millis(want_ms);
    let slack = Duration::from_secs_f32(1.0 / 60.0) + Duration::from_millis(1);
    assert!(got < Duration::MAX, "予約されていない (want {want:?})");
    assert!(got <= want, "予約が要求より遅い: {got:?} > {want:?}");
    assert!(
        got + slack >= want,
        "予約が要求より早すぎる: {got:?} + {slack:?} < {want:?}"
    );
}

/// 稼働中 (エージェントが走っている) なら再描画が要る。
#[test]
fn ヘッドレスで稼働中なら再描画が要求される() {
    let s = IdleSignals {
        agents_running: true,
        ..idle()
    };
    assert_scheduled(drive(s, 4), IDLE_AGENT_MS);
}

/// 承認待ち・応答待ちがあるなら、いちばん短い刻みで回る。
#[test]
fn ヘッドレスで応答待ちなら短い刻みで回る() {
    let s = IdleSignals {
        awaiting: true,
        ..idle()
    };
    assert_scheduled(drive(s, 4), IDLE_AWAITING_MS);
}

/// 家事 (外部変更の見張り) だけがあるときは、低頻度で回る。
/// = 「全停止なら要らない」の対偶側を固定する。
#[test]
fn ヘッドレスで家事だけなら低頻度で回る() {
    let s = IdleSignals {
        watching_files: true,
        ..idle()
    };
    assert_scheduled(drive(s, 4), IDLE_HOUSEKEEP_MS);
}

/// 優先順位: 応答待ち > アニメ > エージェント > 家事。
#[test]
fn priority_order_is_stable() {
    let all = IdleSignals {
        had_input: true,
        animating: true,
        awaiting: true,
        agents_running: true,
        watching_files: true,
        timers_due: true,
        focused: true,
        visible: true,
    };
    assert_eq!(idle_repaint_ms(all), Some(IDLE_AWAITING_MS));
    assert_eq!(
        idle_repaint_ms(IdleSignals {
            awaiting: false,
            ..all
        }),
        None
    );
    assert_eq!(
        idle_repaint_ms(IdleSignals {
            awaiting: false,
            animating: false,
            ..all
        }),
        Some(IDLE_AGENT_MS)
    );
    assert_eq!(
        idle_repaint_ms(IdleSignals {
            awaiting: false,
            animating: false,
            agents_running: false,
            ..all
        }),
        Some(IDLE_HOUSEKEEP_MS)
    );
}

/// 予約は必ず「粗い方が緩い」向きに並んでいる (逆転すると背面の方が重くなる)。
#[test]
fn intervals_are_monotonic() {
    assert!(IDLE_AWAITING_MS < IDLE_AGENT_MS);
    assert!(IDLE_AGENT_MS < IDLE_AGENT_BACKGROUND_MS);
    assert!(IDLE_AGENT_BACKGROUND_MS <= IDLE_HOUSEKEEP_MS);
    assert!(IDLE_HOUSEKEEP_MS < IDLE_BACKGROUND_MS);
    assert!(IDLE_BACKGROUND_MS < IDLE_HIDDEN_MS);
}
