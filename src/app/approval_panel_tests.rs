use super::*;
use crate::agents::approvals::{
    ApprovalKind, ApprovalQueue, Command, Decision, Policy, ReplyAction, Scope, Verdict,
};

fn queue() -> ApprovalQueue {
    let dir = crate::test_util::unique_temp_dir("zaivern-approvals", "panel");
    ApprovalQueue::in_dir(dir)
}

/// キー 1 打 → コマンドの対応 (承認パネルの操作の要)。
#[test]
fn キーが承認コマンドへ正しく割り当たる() {
    use egui::Key;
    let k = panels::approval_key_command;
    assert_eq!(k(Key::Y, false), Some(Command::Approve));
    assert_eq!(k(Key::Y, true), Some(Command::Approve));
    assert_eq!(k(Key::A, false), Some(Command::ApproveAllOfKind));
    assert_eq!(k(Key::A, true), Some(Command::ApproveKindForAgentAlways));
    assert_eq!(k(Key::N, false), Some(Command::Deny));
    assert_eq!(k(Key::N, true), Some(Command::DenyKindForAgentAlways));
    // 割り当てていないキーは食べない (下の端末へそのまま流れる)
    assert_eq!(k(Key::J, false), None);
    assert_eq!(k(Key::Escape, false), None);
}

/// 承認の応答は**その要求を出したセッションだけ**へ向く。
/// (全員へ撒くと、別のエージェントに勝手な YES を撃つ事故になる)
#[test]
fn 応答の宛先は要求元のセッションだけ() {
    let mut q = queue();
    let Verdict::Queued { id } = q.intake(7, Some("claude"), "Do you want to create foo.txt?", 111)
    else {
        panic!("積まれるはず");
    };
    // 別のセッションにも待ちを作る
    let _ = q.intake(9, Some("codex"), "Do you want to create bar.txt?", 222);

    let res = q.apply(id, Command::Approve);
    assert_eq!(res.replies.len(), 1, "1 件だけに応える");
    assert_eq!(res.replies[0].0, 7, "宛先はセッション 7");
    assert_eq!(res.replies[0].1, ReplyAction::Approve);
    assert_eq!(q.pending_len(), 1, "もう一方は残る");
}

/// 応答は「セッション ID 一致」で配る — 番号 (index) では配らない。
/// index はセッションを閉じるとずれ、他人に YES を撃ってしまう。
#[test]
fn 応答の配達はidで引く() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn resolve_approval(")
        .nth(1)
        .expect("実行部がある");
    let head = &body[..body.find("\n    /// ").unwrap_or(body.len())];
    assert!(
        head.contains("self.agents.sessions.iter_mut().find(|s| s.id == *sid)"),
        "セッション ID で引いていない"
    );
    assert!(
        head.contains("press_pet_approve_button"),
        "承認キーの経路が無い"
    );
    assert!(head.contains("resolve_attention"), "拒否後の後始末が無い");
}

/// 生成されたポリシーは config.toml の `[[approval_policies]]` として
/// **読み戻せる形**で書かれる (書けても読めなければ意味がない)。
#[test]
fn ポリシーはconfig_tomlへ往復できる形で書かれる() {
    let p = Policy {
        kind: ApprovalKind::FileWrite,
        scope: Scope::Agent("cl\"aude".into()),
        decision: Decision::AllowAlways,
    };
    let (scope, target) = p.scope.to_toml();
    let entry = config::ApprovalPolicy {
        kind: p.kind.as_str().to_string(),
        scope: scope.to_string(),
        target,
        decision: p.decision.as_str().to_string(),
    };
    let toml_text = render_approval_policy(&entry);
    // 引用符入りの名前でも壊れた TOML にならない
    let cfg: config::Config = toml::from_str(&toml_text).expect("読み戻せる TOML");
    assert_eq!(cfg.approval_policies.len(), 1);
    let back = config::approval_policies_from_config(&cfg);
    assert_eq!(back.len(), 1, "エンジンの Policy へ戻せる");
    assert_eq!(back[0], p, "往復して同じポリシーになる");
}

/// 権限昇格は「常に許可」にできない。UI はそれを**黙って握り潰さない**。
#[test]
fn 権限昇格の常に許可は断られてui_が伝える() {
    let mut q = queue();
    // sudo を含む文面は privilege (never_auto) として分類される
    let Verdict::Queued { id } = q.intake(3, Some("claude"), "Run sudo rm -rf /tmp/x ?", 42) else {
        panic!("積まれるはず");
    };
    assert!(
        q.get(id).map(|r| r.never_auto).unwrap_or(false),
        "権限昇格として分類される"
    );

    let res = q.apply(id, Command::ApproveKindForAgentAlways);
    assert!(res.refused_always, "常に許可は断られる");
    assert!(res.policy.is_none(), "ポリシーは作られない");
    assert_eq!(res.replies.len(), 1, "この 1 件だけは承認される");

    // app.rs がその事実を必ず画面へ出していること
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn resolve_approval(")
        .nth(1)
        .expect("実行部がある");
    assert!(
        body.contains("if res.refused_always"),
        "refused_always を見ていない"
    );
    assert!(
        body.contains("権限昇格は「常に許可」にできません"),
        "利用者へ伝える文言が無い"
    );
}

/// 監査ログは**描画中に読まない**。控えが無いときだけ描画の外で 1 回読む。
#[test]
fn 監査ログは毎フレーム読まない() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn terminal_panel(")
        .nth(1)
        .expect("パネルがある");
    let head = &body[..body.find("\n    /// ").unwrap_or(body.len())];
    assert!(
        head.contains("self.approvals_audit_cache.is_none()"),
        "控えの有無を見ずに読み直している"
    );
    assert!(head.contains("read_audit_tail"), "監査ログの読み口が無い");
    // 読み込みはパネルのクロージャの**外**にある = 描画中の I/O ではない
    let show = head
        .find(".show_animated(ctx, show,")
        .expect("パネル描画がある");
    let read = head.find("read_audit_tail").expect("読み口がある");
    assert!(read > show, "描画クロージャの中で読んでいる可能性がある");
}
