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
