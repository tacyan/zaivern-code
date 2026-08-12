use crate::agent_input::{AgentInputBuffer, ComposerTarget};

/// 全員宛てと 1 体宛てが**別の経路**へ落ちる。
/// ここが混ざると「レビュー指示が全エージェントへ漏れる」に戻る。
#[test]
fn 全員宛てと指名宛てで経路が分かれる() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let head = src
        .split("let composer = panels::agent_composer_ui(")
        .nth(1)
        .expect("コンポーザを呼んでいる");
    let arm = &head[..head.find("\n    /// ").unwrap_or(head.len())];
    assert!(
        arm.contains("panels::ComposerAction::Send(t) => acts.broadcast = Some(t)"),
        "全員宛てが broadcast へ行っていない"
    );
    assert!(
        arm.contains("panels::ComposerAction::SendTo(id, t) => acts.send_to = Some((id, t))"),
        "指名宛てが send_to へ行っていない"
    );
    // 指名宛ての実行部は broadcast を通らない
    let apply = src
        .split("if let Some((id, text)) = acts.send_to {")
        .nth(1)
        .expect("send_to の実行部がある");
    let apply = &apply[..apply.find("if acts.voice_stop").unwrap_or(apply.len())];
    assert!(
        apply.contains("find(|s| s.id == id)"),
        "ID で 1 体を引いていない"
    );
    assert!(
        !apply.contains("broadcast"),
        "指名宛てが一斉送信へ落ちている"
    );
}

/// 死んだエージェントの下書きは掃かれる。
/// (ID は再利用されないが、残しておくと際限なく貯まる)
#[test]
fn 消えたエージェントの下書きは掃かれる() {
    let mut b = AgentInputBuffer::new();
    b.set_draft_for(ComposerTarget::Agent(1), "1 番への指示");
    b.set_draft_for(ComposerTarget::Agent(2), "2 番への指示");
    b.set_draft_for(ComposerTarget::Broadcast, "全員へ");
    assert_eq!(b.draft_for(ComposerTarget::Agent(2)), "2 番への指示");

    // 2 番が死んだ
    b.retain_agents(&[1]);
    assert_eq!(
        b.draft_for(ComposerTarget::Agent(1)),
        "1 番への指示",
        "生きている方は残る"
    );
    assert_eq!(
        b.draft_for(ComposerTarget::Agent(2)),
        "",
        "死んだ方は消える"
    );
    assert_eq!(
        b.draft_for(ComposerTarget::Broadcast),
        "全員へ",
        "全員宛ては消さない"
    );
}

/// 掃除はセッションの増減を拾う 1 か所 (`reconcile_sessions`) で走る。
#[test]
fn 下書きの掃除はセッション増減の場所で走る() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn reconcile_sessions(&mut self) {")
        .nth(1)
        .expect("あるはず");
    let head = &body[..body.find("\n    /// ").unwrap_or(body.len())];
    assert!(head.contains("self.agent_input_buf.retain_agents(&ids)"));
}
