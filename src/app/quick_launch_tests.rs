use super::*;
use crate::config::{quick_launch_names, quick_launch_slots, AgentPreset, QUICK_LAUNCH_SLOTS};

fn presets(names: &[&str]) -> Vec<AgentPreset> {
    names
        .iter()
        .map(|n| AgentPreset {
            name: (*n).to_string(),
            ..Default::default()
        })
        .collect()
}

// ── 割り当ての決まり方 ───────────────────────────────────────
#[test]
fn 既定はプリセットの並びの先頭から九件() {
    let ps = presets(&["a", "b", "c"]);
    assert_eq!(quick_launch_slots(&ps, None), vec![0, 1, 2]);
    let many: Vec<String> = (0..20).map(|i| format!("p{i}")).collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    let ps = presets(&refs);
    assert_eq!(quick_launch_slots(&ps, None).len(), QUICK_LAUNCH_SLOTS);
}

#[test]
fn 保存した並びがそのまま番号になる() {
    let ps = presets(&["a", "b", "c"]);
    let stored = ["c".to_string(), "a".to_string()];
    assert_eq!(quick_launch_slots(&ps, Some(&stored)), vec![2, 0]);
}

#[test]
fn 空の割り当ては空のまま() {
    // ユーザーが全部外した状態。既定へ勝手に戻さない (= 起動バーは 0 件)。
    let ps = presets(&["a", "b"]);
    assert!(quick_launch_slots(&ps, Some(&[])).is_empty());
}

#[test]
fn 壊れた設定でもpanicしない() {
    let ps = presets(&["a", "b"]);
    // 消えたプリセット名 / 重複 / 空文字 / 9 件超 — どれも落ちない
    let stored = vec![
        "居ない".to_string(),
        "b".to_string(),
        "b".to_string(),
        String::new(),
        "a".to_string(),
    ];
    assert_eq!(quick_launch_slots(&ps, Some(&stored)), vec![1, 0]);
    // プリセットが 0 件でも落ちない
    assert!(quick_launch_slots(&[], Some(&stored)).is_empty());
    assert!(quick_launch_slots(&[], None).is_empty());
    let over: Vec<String> = (0..50).map(|i| format!("p{i}")).collect();
    let many: Vec<String> = (0..50).map(|i| format!("p{i}")).collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    assert_eq!(
        quick_launch_slots(&presets(&refs), Some(&over)).len(),
        QUICK_LAUNCH_SLOTS
    );
}

#[test]
fn 割り当ての永続化は往復しても順序が保たれる() {
    let ps = presets(&["a", "b", "c", "d"]);
    let slots = vec![3usize, 0, 2];
    let saved = quick_launch_names(&ps, &slots);
    assert_eq!(saved, vec!["d", "a", "c"]);
    // 保存 → 読み込み → もう一度保存、で 1 回も並びが動かない
    let back = quick_launch_slots(&ps, Some(&saved));
    assert_eq!(back, slots, "読み込みで順序が変わった");
    assert_eq!(
        quick_launch_names(&ps, &back),
        saved,
        "再保存で順序が変わった"
    );
}

/// **番号は使用頻度や通知で動かない。**
///
/// 構造検査: 番号を決める関数の入力は「プリセット一覧」と「保存済みの並び」
/// しか無く、本体に並べ替えも頻度・未読・通知の参照も無い。
/// (cmux が HN で「通知順で並べ替えたら ⌘1-9 の割当が動き続ける」と
/// 批判された轍を、構造として踏めないようにしておく)
#[test]
fn 番号は使用頻度や通知で変わらない() {
    let src = include_str!("../config.rs").replace("\r\n", "\n");
    let body = src
        .split("pub fn quick_launch_slots(")
        .nth(1)
        .expect("quick_launch_slots がある");
    let body = body.split("\n}\n").next().expect("関数の終わり");
    for banned in [
        "sort",
        "reverse",
        "unread",
        "recent",
        "notif",
        "count",
        "usage",
        "rank",
        "score",
        "last_used",
        "activity",
    ] {
        assert!(
            !body.contains(banned),
            "番号の決定に {banned} が混ざっている: 番号が動く"
        );
    }
    // 引数は 2 つだけ (プリセット一覧 + 保存済みの並び)。
    let sig = src
        .split("pub fn quick_launch_slots(")
        .nth(1)
        .and_then(|b| b.split(')').next())
        .expect("シグネチャ");
    assert_eq!(sig.matches(':').count(), 2, "入力が増えている: {sig}");

    // 同じ入力なら何度呼んでも同じ並び (呼び出し回数で動かない)。
    let ps = presets(&["a", "b", "c"]);
    let stored = ["c".to_string(), "b".to_string(), "a".to_string()];
    let first = quick_launch_slots(&ps, Some(&stored));
    for _ in 0..10 {
        assert_eq!(quick_launch_slots(&ps, Some(&stored)), first);
    }

    // 打鍵 → スロット番号の対応も固定 (keybinds 側)。
    for n in 1..=9usize {
        let a = crate::keybinds::quick_launch_action(n).expect("1〜9 はある");
        assert_eq!(crate::keybinds::quick_launch_slot(a), Some(n));
    }
    assert!(crate::keybinds::quick_launch_action(0).is_none());
    assert!(crate::keybinds::quick_launch_action(10).is_none());
}

/// 起動バーは **FocusPane より先に**消費される。
/// (他 OS では ⌃⌥1 が ⌘1 のパターンにも一致するため、順序が逆だと
///  エディタのペイン移動に化ける)
#[test]
fn 起動バーはペイン移動より先に消費される() {
    let src = crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn handle_shortcuts(&mut self, ctx: &egui::Context) {")
        .nth(1)
        .expect("handle_shortcuts がある");
    let quick = body
        .find("BindAction::QuickLaunch1)")
        .expect("起動バーを消費していない");
    let pane = body
        .find("BindAction::FocusPane1)")
        .expect("ペイン移動を消費していない");
    assert!(quick < pane, "起動バーの消費がペイン移動より後ろにある");
}

// ── レイアウト (どの幅でも見切れない / 空なら高さゼロ) ────────
fn label_ws(n: usize, w: f32) -> Vec<f32> {
    vec![w; n]
}

#[test]
fn 割り当てが無いときは一ピクセルも取らない() {
    let plan = quick_bar_plan(1200.0, &[]);
    assert_eq!(plan.shown, 0);
    assert_eq!(plan.height, 0.0, "空なのに高さを取っている");
    assert_eq!(plan.used_w(), 0.0);
    // 幅が 0 でも落ちない
    let plan = quick_bar_plan(0.0, &label_ws(3, 80.0));
    assert_eq!(plan.height, 0.0);
    assert_eq!(plan.shown, 0);
}

/// 極端なサイズで **全ての矩形が可用領域に収まり、重ならない**。
#[test]
fn どの幅でもチップは収まり重ならない() {
    // (可用幅, 件数, ラベル実寸)
    let cases = [
        (1200.0_f32, 9usize, 110.0_f32), // 1200x300 相当の広い画面
        (1200.0, 3, 40.0),
        (900.0, 9, 110.0), // 900x700
        (900.0, 6, 60.0),
        (400.0, 9, 110.0), // 400x700 (最狭)
        (400.0, 2, 200.0),
        (120.0, 9, 90.0), // サイドバーを開き切った極端な幅
        (40.0, 5, 90.0),  // 1 個も入らない
    ];
    for (avail, n, w) in cases {
        let plan = quick_bar_plan(avail, &label_ws(n, w));
        assert!(plan.shown <= n, "{avail}x{n}: 件数より多く描いている");
        assert!(
            plan.used_w() <= avail + 0.01,
            "{avail}x{n}: 行が可用幅を超える ({} > {avail})",
            plan.used_w()
        );
        if plan.shown == 0 {
            assert_eq!(plan.height, 0.0, "{avail}x{n}: 0 件なのに高さを取っている");
            continue;
        }
        assert!(plan.height > 0.0);
        assert!(plan.chip_w > 0.0);
        // 全ての矩形が可用領域に収まり、互いに重ならない
        for i in 0..plan.shown {
            let (x0, x1) = (plan.chip_x(i), plan.chip_x(i) + plan.chip_w);
            assert!(x0 >= 0.0, "{avail}x{n}: 左へはみ出した");
            assert!(x1 <= avail + 0.01, "{avail}x{n}: 右へはみ出した ({x1})");
            if i + 1 < plan.shown {
                assert!(
                    x1 <= plan.chip_x(i + 1) + 0.01,
                    "{avail}x{n}: チップ {i} と {} が重なる",
                    i + 1
                );
            }
        }
    }
}

#[test]
fn 狭いときだけアイコンへ縮退する() {
    let wide = quick_bar_plan(1200.0, &label_ws(4, 90.0));
    assert!(!wide.icons_only, "広いのに縮退している");
    let narrow = quick_bar_plan(260.0, &label_ws(4, 90.0));
    assert!(narrow.icons_only, "狭いのに縮退していない");
    assert_eq!(narrow.shown, 4, "縮退すれば全部入るはず");
}

// ── 自動命名の判断 (純関数) ─────────────────────────────────
fn ready() -> AutoNameSignals {
    AutoNameSignals {
        enabled: true,
        turn_ended: true,
        running: true,
        manual: false,
        has_generator: true,
        has_brief: true,
        already_named: false,
    }
}

#[test]
fn 自動命名の既定はオフ() {
    // Config の既定 (config.rs 側) と、判断関数の既定の両方を固定する。
    assert!(
        !crate::config::Config::default().auto_name_sessions,
        "既定でオンになっている"
    );
    assert!(!should_auto_name(AutoNameSignals::default()));
    assert!(!should_auto_name(AutoNameSignals {
        enabled: false,
        ..ready()
    }));
}

#[test]
fn 自動命名はターン終了時にだけ走る() {
    assert!(should_auto_name(ready()));
    assert!(!should_auto_name(AutoNameSignals {
        turn_ended: false,
        ..ready()
    }));
    // 同じ材料で二度は走らせない
    assert!(!should_auto_name(AutoNameSignals {
        already_named: true,
        ..ready()
    }));
    // 終了済み / 対応 CLI でない / 材料が無い、も走らせない
    for s in [
        AutoNameSignals {
            running: false,
            ..ready()
        },
        AutoNameSignals {
            has_generator: false,
            ..ready()
        },
        AutoNameSignals {
            has_brief: false,
            ..ready()
        },
    ] {
        assert!(!should_auto_name(s), "{s:?} で走ってしまう");
    }
}

#[test]
fn 手動名が常に勝つ() {
    // 依頼の段でも撃たない
    assert!(!should_auto_name(AutoNameSignals {
        manual: true,
        ..ready()
    }));
    // 走らせている間に手で付けられた場合も、結果を捨てて手動名を残す
    assert_eq!(
        apply_named_title("わたしの名前", true, Some("Auto Title".into())),
        "わたしの名前"
    );
}

#[test]
fn 生成に失敗したら従来の名前のまま() {
    assert_eq!(
        apply_named_title("Claude Code #2", false, None),
        "Claude Code #2"
    );
    // 空 / 空白だけの結果も従来名のまま (検疫をすり抜けた場合の二重の栓)
    assert_eq!(
        apply_named_title("Claude Code #2", false, Some(String::new())),
        "Claude Code #2"
    );
    assert_eq!(
        apply_named_title("Claude Code #2", false, Some("   ".into())),
        "Claude Code #2"
    );
    // まともな題名は反映される
    assert_eq!(
        apply_named_title("Claude Code #2", false, Some("ログイン修正".into())),
        "ログイン修正"
    );
}

/// 自動命名は **そのセッション自身の CLI** しか呼ばない。
/// (別のエージェントへ投げない、を構造で固定する)
#[test]
fn 命名は自分自身のcliへしか投げない() {
    let src = crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn auto_name_tick(&mut self, ctx: &egui::Context) {")
        .nth(1)
        .expect("auto_name_tick がある");
    let body = body.split("\n    }\n").next().expect("関数の終わり");
    assert!(
        body.contains("title_generator_for_command(&s.command)"),
        "命名器をそのセッションのコマンドから引いていない"
    );
    for banned in [
        "super_agent",
        "diagnostician",
        "supervisor",
        "AGENT_CATALOG",
    ] {
        assert!(
            !body.contains(banned),
            "別の相手へ投げる経路がある: {banned}"
        );
    }
}
