fn src() -> String {
    crate::app::SRC.replace("\r\n", "\n")
}

/// 関数 1 本ぶんの本文を、次の同じインデントの `fn ` まで切り出す。
fn body_of(sig: &str) -> String {
    let after = src()
        .split(sig)
        .nth(1)
        .unwrap_or_else(|| panic!("{sig} が無い"))
        .to_string();
    let end = crate::app::method_end(&after);
    if end < after.len() {
        after[..end].to_string()
    } else {
        after.chars().take(4000).collect()
    }
}

/// 送信経路は 1 本なので、見張りもそこ 1 か所で足りる。
#[test]
fn 送信経路はコスト上限の門を通る() {
    for sig in [
        "fn queue_submit(&mut self, mut job: submit::Job) -> bool {",
        "fn queue_submit_all(&mut self, text: &str) -> Option<usize> {",
        "fn queue_submit_stalled(&mut self, text: &str) -> Option<usize> {",
    ] {
        let body = body_of(sig);
        assert!(
            body.contains("self.cost_block_reason()"),
            "{sig} がコスト上限の門を通っていない"
        );
    }
}

/// 外から届く確定送信 (`/api/voice` = `zai session send` / `/api/bulk` /
/// `/api/term` / `/api/prompt` とプラグイン) は配達機構へ合流させる。
/// 本文と CR を 1 回で書くと、長い本文ではペースト扱いで CR が飲まれて
/// 送信されない。合流していればコスト上限の門も一緒に通る。
#[test]
fn リモートの確定送信は配達機構を通る() {
    for sig in [
        "fn remote_reply_voice_send(&mut self, text: &str, id: i64, submit: bool) -> String {",
        "fn remote_reply_bulk(",
        "fn remote_reply_term_input(&mut self, payload: &str, raw: bool) -> String {",
        "fn send_agent_prompt(",
    ] {
        let body = body_of(sig);
        assert!(
            !body.contains(r#"\r")"#),
            "{sig} が本文と CR を 1 回で書いている (長文が送信されない)"
        );
        assert!(
            body.contains("self.queue_submit"),
            "{sig} の確定送信が配達機構 (queue_submit*) を通っていない"
        );
    }
    let voice = body_of(
        "fn remote_reply_voice_send(&mut self, text: &str, id: i64, submit: bool) -> String {",
    );
    assert!(
        voice.contains("self.cost_block_reason()"),
        "確定送信の理由をリモートへ返していない"
    );
}

/// **黙って無視しない** — 止めたときは必ず理由を画面へ出す。
#[test]
fn 止めた理由を必ず画面に出す() {
    let body = body_of("fn queue_submit(&mut self, mut job: submit::Job) -> bool {");
    assert!(
        body.contains("if let Some(why) = self.cost_block_reason() {")
            && body.contains("self.toast(why, false);"),
        "止めた理由をトーストで出していない"
    );
}

/// 上限を設定していないときは 1 ピクセルも出さない。
#[test]
fn 上限が未設定ならバッジを作らない() {
    let body = body_of("fn cost_badge(&self) -> Option<(String, String, egui::Color32)> {");
    assert!(
        body.contains("let st = self.cost_alert.as_ref()?;"),
        "判定が無いときに None を返す門が消えた (常に 0 を出すバッジになる)"
    );
    let tick = body_of("fn cost_limit_tick(&mut self) {");
    assert!(
        tick.contains("if !limits.any() {") && tick.contains("self.cost_alert = None;"),
        "上限が未設定でも判定結果を残している"
    );
}

/// アイドル時のコストはゼロ — 集計か設定が変わったときだけ計算し直す。
#[test]
fn 上限の判定は集計か設定が変わったときだけ走る() {
    let tick = body_of("fn cost_limit_tick(&mut self) {");
    assert!(
        tick.contains("if self.cost_stamp == Some(stamp) {") && tick.contains("return;"),
        "毎フレーム推定コストを計算し直している"
    );
    assert!(
        tick.contains("self.cost_gate.changed(Self::COST_GATE_ID, &key)"),
        "通知が門番 (EdgeGate) を通っていない = 毎回鳴る"
    );
}
