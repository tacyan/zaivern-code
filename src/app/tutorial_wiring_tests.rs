use super::*;

/// ツアーが光らせようとする**すべての** `AnchorId` について、
/// app.rs のどこかで `tutorial::anchor(..)` を呼んでいること。
///
/// 手順表に新しいアンカーを足したのに UI 側の申告を忘れると、
/// その手順は 4 秒フォールバック表示されてから勝手に飛ぶ (= 説明が消える)。
/// 変種名は `Debug` から取るので、名前を変えてもこのテストは追随する。
#[test]
fn 手順表の全アンカーがapp_rsで申告されている() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let mut missing: Vec<String> = Vec::new();
    for step in crate::tutorial::STEPS {
        let Some(id) = step.anchor else { continue };
        let needle = format!("AnchorId::{id:?}");
        if !src.contains(&needle) {
            missing.push(format!("{} ({needle})", step.id));
        }
    }
    assert!(
        missing.is_empty(),
        "アンカーを申告していない手順がある: {missing:?}"
    );
}

/// 手順表が要求する依頼はすべて `apply_tutorial_action` に届く。
///
/// `match` は網羅なのでコンパイラも守ってくれるが、
/// 「腕はあるが中身が空」を防ぐため、変種ごとに本体の目印も見る。
#[test]
fn 手順表の全依頼にルーティングがある() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn apply_tutorial_action(")
        .nth(1)
        .expect("ルーティング関数がある");
    for step in crate::tutorial::STEPS {
        let Some(act) = step.pre_action else { continue };
        let name = format!("{act:?}");
        // 変種名だけ取り出す (`OpenSidebar(Files)` → `OpenSidebar`)
        let variant = name.split('(').next().unwrap_or(&name).to_string();
        assert!(
            body.contains(&format!("TA::{variant}")),
            "{} の依頼 {variant} を実行していない",
            step.id
        );
    }
}

/// サイドバーの依頼はすべて実在するタブに落ちる (対応表の網羅)。
#[test]
fn サイドバーの依頼がタブへ一意に対応する() {
    use crate::tutorial::SidebarTarget as S;
    let pairs = [
        (S::Files, SidebarTab::Files),
        (S::Search, SidebarTab::Search),
        (S::Agents, SidebarTab::Agents),
        (S::Sessions, SidebarTab::Sessions),
        (S::Plugins, SidebarTab::Plugins),
        (S::Git, SidebarTab::Git),
        (S::GitHub, SidebarTab::GitHub),
    ];
    for (t, want) in pairs {
        assert!(sidebar_tab_for(t) == want, "{t:?} の対応が違う");
    }
}

/// 自動開始は**一度だけ**。2 回目は false で、位置も勝手に戻らない。
#[test]
fn 自動開始は一度だけ() {
    let dir = crate::test_util::unique_temp_dir("zaivern-tutorial", "autostart-once");
    let mut t = crate::tutorial::Tutorial::in_dir(dir.clone());
    assert!(t.autostart(), "初回は開始する");
    assert!(t.active());
    // 見終わる (= 既読フラグが立つ)
    t.skip();
    // 新しいインスタンス = 次回起動。もう出ない。
    let mut t2 = crate::tutorial::Tutorial::in_dir(dir.clone());
    assert!(!t2.autostart(), "2 度目は自動開始しない");
    assert!(!t2.active());
    // 「チュートリアルを再開」だけは既読でも開く
    t2.restart();
    assert!(t2.active() && t2.index() == 0);
}

/// ツアーは `tutorial_autostarted` で 1 回に絞ってから呼ぶ
/// (毎フレーム `autostart()` を呼ぶと位置が 0 に戻り続けて操作できない)。
#[test]
fn 自動開始の呼び出しはフラグで守られている() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn tutorial_tick(")
        .nth(1)
        .expect("tutorial_tick がある");
    let head = &body[..body.find("fn ").unwrap_or(body.len())];
    assert!(head.contains("if !self.tutorial_autostarted"));
    assert!(head.contains("self.tutorial_autostarted = true;"));
    assert!(head.contains("self.tutorial.overlay(ctx, &theme, &self.keys)"));
}

/// オーバーレイはアイドル判定の**前**に呼ぶ。
///
/// 逆順だと `IdleSignals::animating` にツアーの 30fps 予約が乗らず、
/// 「アニメ中なのに寝る」= リングがカクつく。ここが逆になったら気付けるように。
#[test]
fn ツアーはアイドル判定より先に描く() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn update_impl(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {")
        .nth(1)
        .expect("update_impl がある");
    let tick = body
        .find("self.tutorial_tick(ctx);")
        .expect("ツアーを描いている");
    let idle = body
        .find("self.schedule_idle_repaint(ctx);")
        .expect("アイドル予約がある");
    assert!(tick < idle, "ツアーはアイドル予約より先でなければならない");
}
