fn follow_tick_body() -> String {
    let src = crate::app::SRC.replace("\r\n", "\n");
    let after = src
        .split("fn follow_tick(&mut self, ctx: &egui::Context) {")
        .nth(1)
        .expect("follow_tick が無い")
        .to_string();
    after.chars().take(3000).collect()
}

#[test]
fn 追従がオフなら最初の1行で降りる() {
    let body = follow_tick_body();
    let head: String = body.chars().take(120).collect();
    assert!(
        head.contains("if !self.follow.is_on() {"),
        "オフで即戻る門が消えた (アイドルで git を叩くようになる)"
    );
}

#[test]
fn 追従は未オープンのファイルをプレビュータブで開く() {
    let body = follow_tick_body();
    assert!(
        body.contains("self.open_path_preview(&spot.path);"),
        "追従が確定タブを増やしている"
    );
    assert!(
        !body.contains("self.open_path(&spot.path)"),
        "プレビュータブを迂回している"
    );
}

#[test]
fn 追従はユーザーのスクロールで一時停止する() {
    let body = follow_tick_body();
    assert!(
        body.contains("raw_scroll_delta") && body.contains("note_user_scroll()"),
        "ユーザーのスクロールで止まる経路が消えた"
    );
}

#[test]
fn 追従はエディタを見ていないフレームでは走らない() {
    let body = follow_tick_body();
    assert!(
        body.contains("if self.center != CenterView::Editor {"),
        "見えていない画面のために git を起こしている"
    );
}

#[test]
fn 追従の走査は別スレッドで行う() {
    let body = follow_tick_body();
    assert!(
        body.contains("std::thread::spawn"),
        "UI スレッドで git を待っている"
    );
}

#[test]
fn 通知は稼働中から待機の遷移だけを通す() {
    let src = crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn notify_work_done(&mut self, win_focused: bool) {")
        .nth(1)
        .expect("notify_work_done が無い");
    let body: String = body.chars().take(2000).collect();
    assert!(
        body.contains("self.work_gate.note("),
        "遷移エッジの門番を通っていない"
    );
    assert!(
        body.contains("self.supervisor.notify_phase_of(s.id, s.running())"),
        "段を画面から推測している (設計原則 4 違反)"
    );
}
