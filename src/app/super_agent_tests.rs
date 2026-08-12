use super::*;
use crate::agents::{Approval, AGENT_CATALOG};

/// DoD: 既定は「なし」。何も設定していない人が、知らないうちに自分の
/// エージェントを外部プロセスへ相談させられることは無い。
#[test]
fn 既定の監視役はなし() {
    let c = Config::default();
    assert_eq!(c.super_agent.command, "");
    assert!(!c.super_agent.enabled);
    assert_eq!(c.super_agent.active_command(), None);
    // 相談相手が居ないので LLM エスカレーションも当然 OFF
    assert!(!c.supervisor.llm_escalation);
}

/// 素のシェル (コマンド空) は指揮官にできない。注入行がそのまま
/// シェルコマンドとして実行されてしまうため、これだけは必ず弾く。
#[test]
fn 素のシェルは指揮官に選べない() {
    assert!(commander_reject_reason("").is_some());
    assert!(commander_reject_reason("   ").is_some());
}

/// DoD: 起動しているエージェントなら **どれでも** 指揮官に選べる。
/// 指揮は端末内で完結するので、カタログ登録もヘッドレス対応も要らない。
/// カタログ外のカスタムプリセットも、ユーザーがエージェントとして
/// 登録したものなら候補になる。
#[test]
fn どのエージェントでも指揮官に選べる() {
    for cmd in ["my-custom-agent", "python3 my_agent.py"] {
        assert!(
            commander_reject_reason(cmd).is_none(),
            "{cmd} はカタログ外でも指揮官候補であるべき"
        );
    }
    for spec in AGENT_CATALOG {
        assert!(
            commander_reject_reason(spec.bin).is_none(),
            "{} は headless={:?} に関わらず指揮官候補であるべき",
            spec.bin,
            spec.headless
        );
    }
}

/// 既定のプリセット一覧から、実際に指揮官として出せるものが 1 つ以上ある。
#[test]
fn 既定プリセットに指揮官候補が存在する() {
    let c = Config::default();
    let picks: Vec<&str> = c
        .agents
        .iter()
        .filter(|p| commander_reject_reason(&p.command).is_none())
        .map(|p| p.command.as_str())
        .collect();
    assert!(!picks.is_empty(), "既定プリセットに候補が無いのはおかしい");
    // 素のシェル (コマンド空) は必ず候補外
    assert!(!picks.iter().any(|c| c.trim().is_empty()));
}

/// ピッカーで足したプリセットが、そのまま指揮官の候補一覧に載ること。
/// 「追加はできたが指揮官には選べない」という中途半端な状態を防ぐ。
#[test]
fn ピッカーで足したプリセットは指揮官候補に載る() {
    for spec in crate::agents::AGENT_CATALOG {
        let p = crate::agent_picker::plain_preset(spec);
        assert!(
            commander_reject_reason(&p.command).is_none(),
            "{}: 追加したプリセットが指揮官候補に載らない",
            spec.bin
        );
    }
}

/// ピッカーで足したプリセットに、承認モードがちゃんと効くこと。
/// (足せても全自動/承認の切替が効かなければ、壊れた項目でしかない)
#[test]
fn ピッカーで足したプリセットに承認モードが効く() {
    use crate::agents::{apply_approval, Approval};
    for spec in crate::agents::AGENT_CATALOG {
        let p = crate::agent_picker::plain_preset(spec);
        // Auto: フラグを持つ CLI なら必ず付与される
        let auto = apply_approval(&p.command, Approval::Auto);
        if !spec.auto_flag.is_empty() {
            assert!(
                auto.contains(spec.auto_flag),
                "{}: Auto にしても自動承認フラグが付かない",
                spec.bin
            );
        }
        // Ask: 全自動プリセットからは必ずフラグが外れる
        if let Some(a) = crate::agent_picker::auto_preset(spec) {
            let ask = apply_approval(&a.command, Approval::Ask);
            if !spec.auto_flag.is_empty() {
                assert!(
                    !ask.contains(spec.auto_flag),
                    "{}: Ask にしても自動承認フラグが残る",
                    spec.bin
                );
            }
        }
    }
}

// ---- 指揮官セッションの対応付け (タイトル指名 / コマンド一致) ----

fn rows() -> Vec<(u64, bool, String, String)> {
    vec![
        (1, true, "codex".into(), "Codex CLI (全自動)".into()),
        (
            2,
            true,
            "claude --dangerously-skip-permissions".into(),
            "Claude Code (全自動)".into(),
        ),
        (3, false, "claude".into(), "Claude Code (全自動) #2".into()),
    ]
}

/// 3 体同じ CLI が並んでいても、タイトルで指名した 1 体だけが指揮官になる。
/// これが「起動中のエージェントから名前で選ぶ」UI の土台。
#[test]
fn タイトル指名で該当セッションだけを拾う() {
    let r: Vec<(u64, bool, String, String)> = vec![
        (
            1,
            true,
            "claude --dangerously-skip-permissions".into(),
            "Claude Code (全自動)".into(),
        ),
        (
            2,
            true,
            "claude --dangerously-skip-permissions".into(),
            "Claude Code (全自動) #2".into(),
        ),
        (
            3,
            true,
            "claude --dangerously-skip-permissions".into(),
            "Claude Code (全自動) #3".into(),
        ),
    ];
    assert_eq!(
        pick_commander_session(
            &r,
            "Claude Code (全自動) #3",
            "claude --dangerously-skip-permissions"
        ),
        Some(3)
    );
}

/// DoD: 指名した相手が居なければ `None`。同じ CLI の別セッションへ勝手に
/// フォールバックしない (「#3 を指名したのに #1 が指揮官になる」事故を防ぐ)。
#[test]
fn 指名相手が居なければフォールバックしない() {
    let r: Vec<(u64, bool, String, String)> = vec![
        (1, true, "claude".into(), "Claude Code (全自動)".into()),
        (3, false, "claude".into(), "Claude Code (全自動) #3".into()),
    ];
    assert_eq!(
        pick_commander_session(&r, "Claude Code (全自動) #3", "claude"),
        None
    );
}

/// DoD: 指名は再起動 (別 ID・同タイトル) をまたいで追従する。
/// ID で固定すると再起動のたびに指名が外れてしまう。
#[test]
fn 指名は再起動で変わったidに追従する() {
    let title = "Claude Code (全自動) #3";
    let a: Vec<(u64, bool, String, String)> = vec![(7, true, "claude".into(), title.into())];
    assert_eq!(pick_commander_session(&a, title, "claude"), Some(7));
    // 再起動で ID が変わっても同じタイトルなら追従
    let b: Vec<(u64, bool, String, String)> = vec![(9, true, "claude".into(), title.into())];
    assert_eq!(pick_commander_session(&b, title, "claude"), Some(9));
}

/// タイトルの前後空白は無視して照合する (state.toml の手編集にも耐える)。
#[test]
fn 指名タイトルの前後空白は無視する() {
    let r: Vec<(u64, bool, String, String)> =
        vec![(1, true, "claude".into(), "Claude Code (全自動)".into())];
    assert_eq!(
        pick_commander_session(&r, "  Claude Code (全自動)  ", "claude"),
        Some(1)
    );
}

/// DoD (旧形式): 指名が空なら、監視役の CLI で動いているセッションを正しく拾う。
/// 権限フラグ付きで起動されていても、同じ CLI なら自分自身と見なす。
#[test]
fn 指名なしならフラグ違いでも同じcliとして拾う() {
    assert_eq!(pick_commander_session(&rows(), "", "claude"), Some(2));
    assert_eq!(pick_commander_session(&rows(), "", "codex"), Some(1));
}

/// 動いていないセッションは指揮官として登録しない (もう詰まりようがない)。
#[test]
fn 終了済みセッションは指揮官にしない() {
    let r = vec![(3u64, false, "claude".to_string(), "Claude Code".to_string())];
    assert_eq!(pick_commander_session(&r, "", "claude"), None);
    assert_eq!(pick_commander_session(&r, "Claude Code", "claude"), None);
}

/// DoD: 起動 → 停止 → 別 ID で再起動、を通して追従すること。
/// ここが固定されたままだと、生きているセッションの診断を誤って断り続ける。
#[test]
fn 指揮官セッションidは起動と停止をまたいで追従する() {
    // まだ起動していない
    assert_eq!(pick_commander_session(&[], "", "claude"), None);
    // 起動
    let a = vec![(7u64, true, "claude".to_string(), "Claude Code".to_string())];
    assert_eq!(pick_commander_session(&a, "", "claude"), Some(7));
    // 終了
    let b = vec![(7u64, false, "claude".to_string(), "Claude Code".to_string())];
    assert_eq!(pick_commander_session(&b, "", "claude"), None);
    // 別 ID で再起動
    let c = vec![
        (7u64, false, "claude".to_string(), "Claude Code".to_string()),
        (9, true, "claude".into(), "Claude Code".into()),
    ];
    assert_eq!(pick_commander_session(&c, "", "claude"), Some(9));
}

/// 監視役が未選択 (空文字) なら、どのセッションも指揮官にはならない。
#[test]
fn 監視役未選択なら指揮官セッションは無し() {
    assert_eq!(pick_commander_session(&rows(), "", ""), None);
}

/// 選んだ CLI が 1 つも動いていなければ登録しない。
#[test]
fn 該当cliが居なければ指揮官セッションは無し() {
    assert_eq!(pick_commander_session(&rows(), "", "goose"), None);
}

// ---- LLM の助言も同じ確認ゲートを通る ----

fn snap(id: u64) -> supervisor::SessionSnapshot {
    supervisor::SessionSnapshot {
        id,
        title: format!("agent {id}"),
        screen_text: String::new(),
        running: true,
        waiting_approval: false,
        exit_code: None,
        user_typed: false,
        total_output_bytes: None,
        command: "claude".into(),
        cwd: std::path::PathBuf::new(),
        raw_log: None,
        shell: None,
    }
}

/// **安全の要**: LLM が再起動・停止を勧めても、必ず確認ダイアログへ回る。
/// 権限モードが全自動 (Auto) でも例外にしない。
#[test]
fn llm推奨の破壊的操作は必ず確認を通る() {
    for action in [
        supervisor::Intervention::Restart,
        supervisor::Intervention::Halt,
    ] {
        let cfg = supervisor::SupervisorConfig {
            llm_escalation: true,
            ..Default::default()
        };
        let mut sv = supervisor::Supervisor::new(cfg);
        // セッションを認識させる
        sv.tick(&[snap(1)], Approval::Auto);

        let d = supervisor::Diagnosis {
            session_id: 1,
            anomaly: supervisor::Anomaly::Stall,
            summary: "出力が止まっています".into(),
            recommended: action,
        };
        let Some(it) = sv.intent_from_diagnosis(&d, Approval::Auto) else {
            continue;
        };
        assert!(
            it.needs_confirmation,
            "{action:?} は LLM 由来でも確認が必要"
        );
        assert_eq!(
            route_intent(&it),
            IntentRoute::Confirm,
            "{action:?} は確認ダイアログへ回るべき"
        );
    }
}

/// 監視役が選ばれていない (llm_escalation=false) 間は、診断が来ても
/// 意図に変換しない。フックの取り外し口が無くてもここで確実に止まる。
#[test]
fn llm相談offなら診断は意図に変換されない() {
    let cfg = supervisor::SupervisorConfig::default();
    assert!(!cfg.llm_escalation);
    let mut sv = supervisor::Supervisor::new(cfg);
    sv.tick(&[snap(1)], Approval::Auto);
    let d = supervisor::Diagnosis {
        session_id: 1,
        anomaly: supervisor::Anomaly::Stall,
        summary: "止まっています".into(),
        recommended: supervisor::Intervention::Nudge,
    };
    assert!(sv.intent_from_diagnosis(&d, Approval::Auto).is_none());
}

/// DoD: 監視役自身も決定論的な見張りの対象に含まれる (単一障害点にしない)。
/// スナップショットは全セッションから作るので、監視役だけ除外されることは無い。
#[test]
fn 監視役自身も監視対象に含まれる() {
    let cfg = supervisor::SupervisorConfig::default();
    let mut sv = supervisor::Supervisor::new(cfg);
    // 1 番が監視役として選ばれている想定でも、両方を渡して両方が見られる
    sv.tick(&[snap(1), snap(2)], Approval::Ask);
    assert!(
        sv.state_of(1).is_some(),
        "監視役自身の状態も見立てられるべき"
    );
    assert!(sv.state_of(2).is_some());
}

/// Cockpit セル / Agents サイドバー行と同じ「クリックで選択できるコンテナ +
/// 内側のボタン」構造で、ボタンへのクリックがコンテナに奪われないことを保証する。
///
/// egui のヒットテストは同一レイヤーでは「後に登録したウィジェット」が勝つため、
/// 描画後にコンテナ全面へ ui.interact を掛けると内側のボタン・ミニターミナルが
/// 一切クリックできなくなる (v0.3.0 で実際に起きたバグ)。正しくは
/// UiBuilder::sense + scope_builder でコンテナの判定を子より先に登録する。
/// 再現テスト: エージェント (セル) が複数あるとき、別のセルのターミナルを
/// クリックしたらアクティブ (紫枠) がそのセルへ移動すること。
#[test]
fn cockpit_purple_moves_to_clicked_cell_with_multiple_agents() {
    use egui::{pos2, vec2, PointerButton, Pos2, Rect};

    let ctx = egui::Context::default();
    let mut active: usize = 0;

    let click = |at: Pos2, pressed: bool| -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(at),
            egui::Event::PointerButton {
                pos: at,
                button: PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    };

    let draw = |active: &mut usize, events: Vec<egui::Event>| -> Vec<Rect> {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(980.0, 420.0))),
            events,
            ..Default::default()
        };
        let mut term_rects = vec![Rect::NOTHING; 2];
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut select: Option<usize> = None;
                egui::ScrollArea::vertical()
                    .id_salt("cockpit-grid")
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for (i, term_rect) in term_rects.iter_mut().enumerate() {
                                let is_active = i == *active;
                                let stroke = if is_active {
                                    egui::Stroke::new(
                                        2.0_f32,
                                        egui::Color32::from_rgb(160, 100, 250),
                                    )
                                } else {
                                    egui::Stroke::new(1.0_f32, egui::Color32::GRAY)
                                };
                                let cell = ui.scope_builder(
                                    egui::UiBuilder::new()
                                        .id_salt(("cockpit-cell-select", i))
                                        .sense(egui::Sense::click()),
                                    |ui| {
                                        egui::Frame::none()
                                            .stroke(stroke)
                                            .inner_margin(egui::Margin::same(8.0))
                                            .show(ui, |ui| {
                                                ui.vertical(|ui| {
                                                    ui.set_width(430.0);
                                                    ui.set_height(160.0);
                                                    ui.horizontal(|ui| {
                                                        ui.label(format!("● agent {i}"));
                                                        let _ = ui.small_button("🎤");
                                                    });
                                                    // ミニターミナル相当: 残り全域 click_and_drag
                                                    let avail = ui.available_size();
                                                    let (rect, resp) = ui.allocate_exact_size(
                                                        avail,
                                                        egui::Sense::click_and_drag(),
                                                    );
                                                    if resp.clicked() || resp.drag_started() {
                                                        resp.request_focus();
                                                    }
                                                    if resp.clicked()
                                                        || resp.drag_started()
                                                        || resp.gained_focus()
                                                    {
                                                        select = Some(i);
                                                    }
                                                    rect
                                                })
                                                .inner
                                            })
                                            .inner
                                    },
                                );
                                *term_rect = cell.inner;
                                if cell.response.clicked()
                                    || (cell.response.contains_pointer()
                                        && ui.input(|inp| inp.pointer.primary_pressed()))
                                {
                                    select = Some(i);
                                }
                            }
                        });
                    });
                if let Some(i) = select {
                    *active = i;
                }
            });
        });
        term_rects
    };

    let rects = draw(&mut active, vec![]);
    assert!(
        rects[0] != rects[1] && rects[1].width() > 50.0,
        "前提: セルが2つ並ぶ"
    );

    // 2 つ目のセルのターミナルをクリック → 紫が 1 へ移動
    let at = rects[1].center();
    let _ = draw(&mut active, click(at, true));
    let _ = draw(&mut active, click(at, false));
    assert_eq!(active, 1, "別セルのターミナルをクリックしたら紫が移動する");

    // 戻す: 1 つ目のセルのターミナルをクリック → 紫が 0 へ移動
    let at = rects[0].center();
    let _ = draw(&mut active, click(at, true));
    let _ = draw(&mut active, click(at, false));
    assert_eq!(active, 0, "元のセルへも移動できる");
}

#[test]
fn cockpit_cell_container_does_not_steal_inner_clicks() {
    use egui::{pos2, vec2, PointerButton, Pos2, Rect};

    let ctx = egui::Context::default();

    // 1 フレーム描いて、コンテナとボタンの実座標を egui に登録させる。
    // クリック判定は「前フレームのウィジェット矩形」に対して行われるため、
    // 押す→離すをそれぞれ別フレームで流す。
    // 戻り値: (ボタンが押された, セル選択が発火した, ボタン/タイトル/セルの矩形)
    let draw = |events: Vec<egui::Event>| -> (bool, bool, Rect, Rect, Rect) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))),
            events,
            ..Default::default()
        };
        let mut out = (false, false, Rect::NOTHING, Rect::NOTHING, Rect::NOTHING);
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let cell = ui.scope_builder(
                    egui::UiBuilder::new()
                        .id_salt("test-cell")
                        .sense(egui::Sense::click()),
                    |ui| {
                        egui::Frame::none()
                            .inner_margin(egui::Margin::same(8.0))
                            .show(ui, |ui| {
                                ui.set_min_size(vec2(300.0, 120.0));
                                // 実セルと同じく、文字選択できるタイトルラベル
                                // (クリックを吸う) も置く
                                let title = ui.label("🤖 agent-title");
                                (ui.button("🎤"), title)
                            })
                            .inner
                    },
                );
                let (btn, title) = cell.inner;
                // 本番 cockpit_ui と同じ選択条件
                let selected = cell.response.clicked()
                    || (cell.response.contains_pointer()
                        && ui.input(|i| i.pointer.primary_pressed()));
                out = (
                    btn.clicked(),
                    selected,
                    btn.rect,
                    title.rect,
                    cell.response.rect,
                );
            });
        });
        out
    };

    let click = |at: Pos2, pressed: bool| -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(at),
            egui::Event::PointerButton {
                pos: at,
                button: PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    };

    let (_, _, btn_rect, title_rect, cell_rect) = draw(vec![]);
    assert!(
        cell_rect.contains_rect(btn_rect) && cell_rect.contains_rect(title_rect),
        "前提: ボタンとタイトルはコンテナの内側にある"
    );

    // ボタンの中心をクリック → ボタンが押され、押した時点で選択も追従する
    let (_, sel_on_press, ..) = draw(click(btn_rect.center(), true));
    let (btn_clicked, ..) = draw(click(btn_rect.center(), false));
    assert!(
        btn_clicked,
        "コンテナ (セル選択) が内側のボタンのクリックを奪ってはいけない"
    );
    assert!(sel_on_press, "ボタンを押した時点で紫枠の選択が追従する");

    // タイトル文字 (文字選択がクリックを吸うラベル) を押しても選択が追従する
    let (_, sel_on_press, ..) = draw(click(title_rect.center(), true));
    let _ = draw(click(title_rect.center(), false));
    assert!(
        sel_on_press,
        "タイトル文字を押してもセル選択 (紫枠) が追従する"
    );

    // ボタンの外 (コンテナの余白) をクリックしても選択できる
    let empty = pos2(cell_rect.max.x - 12.0, cell_rect.max.y - 12.0);
    assert!(cell_rect.contains(empty) && !btn_rect.contains(empty));
    let (_, sel_on_press, ..) = draw(click(empty, true));
    let (btn_clicked, sel_on_release, ..) = draw(click(empty, false));
    assert!(
        sel_on_press && sel_on_release,
        "余白クリックでセルを選択できる"
    );
    assert!(!btn_clicked);
}
