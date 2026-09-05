//! 🗣 エージェント同士の伝言 (通常タブ)。
//!
//! Team Run の中では前から動いていた仕組み
//! ([`crate::features::team::imp::result_parser::check_message`]) を、
//! **普通に並べているエージェントタブへも広げる**。
//!
//! ## 何が起きるか
//!
//! エージェントが画面に
//!
//! ```text
//! [ZAI-TEAM-MSG]
//! {"to": "Codex", "text": "server.js を直したので、テストを回してほしい"}
//! [/ZAI-TEAM-MSG]
//! ```
//!
//! と出すと、Zaivern がそれを拾って**相手のタブへ届ける**。届け方は人が
//! 送るときと同じ経路 (`submit`) — 第 2 の配達路を作らない。
//!
//! ## 宛先の決め方
//!
//! 通常タブには `agent-1` のような ID が無いので、**タブの名前**で指す
//! (`Claude Code` / `Codex` / `Antigravity`)。大文字小文字は無視し、
//! 前方一致も許す (`codex` で `Codex (全自動)` に当たる) — 人が打つ名前は
//! 揺れるので、揺れを吸収するのはこちら側の仕事。
//!
//! `all` は自分以外の全員。**自分自身へは届けない** (自分の画面へ自分の
//! 言葉を流しても何も起きないうえ、無限に往復しうる)。
//!
//! ## 届かなかったときの言い分け
//!
//! 断り文は Team 側と**同じ 1 つ** ([`TalkReject`] = `rp::MessageReject`)。
//! 「相手が居ない」と「相手は居るが自分宛て」を混ぜると、エージェントは
//! 綴りを疑って**同じ宛先を書き直す** (Team 側が実機で 2 回踏んだ)。
//! ここに 2 つめの文面を置くと、同じ状況で説明が食い違う。

use crate::features::team::imp::result_parser as rp;
use std::time::{Duration, SystemTime};

/// submit キューが受け付けなかった伝言を再試行する間隔。
/// 毎フレーム同じコスト警告を出すのを防ぐ。
pub const QUEUE_RETRY_BACKOFF: Duration = Duration::from_secs(30);
const DELIVERY_TAG_PREFIX: &str = "agent-talk:";

/// submit outcome から元の伝言を復元するための識別子。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliveryIdentity {
    pub from: u64,
    pub to: u64,
    pub message_identity: u64,
}

impl DeliveryIdentity {
    pub fn delivered_key(self) -> u64 {
        numeric_key(
            b"agent-talk/delivered/v1",
            &[self.from, self.to, self.message_identity],
        )
    }

    pub fn in_flight_key(self) -> u64 {
        numeric_key(
            b"agent-talk/in-flight/v1",
            &[self.from, self.to, self.message_identity],
        )
    }

    pub fn queue_failure_notice_key(self) -> u64 {
        numeric_key(
            b"agent-talk/queue-failure-notice/v1",
            &[self.from, self.to, self.message_identity],
        )
    }

    pub fn outcome_failure_notice_key(self) -> u64 {
        numeric_key(
            b"agent-talk/outcome-failure-notice/v1",
            &[self.from, self.to, self.message_identity],
        )
    }
}

/// 相手 1 人ぶんの宛先 (セッション ID とタブ名)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub id: u64,
    pub name: String,
}

/// 届ける 1 通。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    /// 送り主のセッション。dedupe に必ず含める。
    pub from: u64,
    /// 宛先のセッション。
    pub to: u64,
    /// 元の `[ZAI-TEAM-MSG]` ブロックの安定した識別子。
    pub message_identity: u64,
    /// 相手の端末へ流す本文 (差出人つき)。
    pub text: String,
}

impl Delivery {
    pub fn identity(&self) -> DeliveryIdentity {
        DeliveryIdentity {
            from: self.from,
            to: self.to,
            message_identity: self.message_identity,
        }
    }

    /// submit が実配送の成功 outcome を返した後だけ記録する配達済みキー。
    pub fn delivered_key(&self) -> u64 {
        self.identity().delivered_key()
    }

    /// submit の最終 outcome を待っている間のキー。
    pub fn in_flight_key(&self) -> u64 {
        self.identity().in_flight_key()
    }

    /// 同じ伝言を 1 再試行スロットで 1 回だけ積むためのキー。
    pub fn attempt_key(&self, retry_slot: u64) -> u64 {
        numeric_key(
            b"agent-talk/attempt/v1",
            &[self.from, self.to, self.message_identity, retry_slot],
        )
    }

    /// キュー拒否の警告を同じ伝言で 1 回だけ表示するキー。
    pub fn queue_failure_notice_key(&self) -> u64 {
        self.identity().queue_failure_notice_key()
    }

    /// submit の outcome をこの伝言へ戻す厳密タグ。
    pub fn delivery_tag(&self) -> String {
        let id = self.identity();
        format!(
            "{DELIVERY_TAG_PREFIX}{}:{}:{}",
            id.from, id.to, id.message_identity
        )
    }
}

/// agent-talk 専用タグを厳密に読む。余分な要素や符号は受け入れない。
pub fn parse_delivery_tag(tag: &str) -> Option<DeliveryIdentity> {
    let mut parts = tag.strip_prefix(DELIVERY_TAG_PREFIX)?.split(':');
    let from = parse_tag_number(parts.next()?)?;
    let to = parse_tag_number(parts.next()?)?;
    let message_identity = parse_tag_number(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some(DeliveryIdentity {
        from,
        to,
        message_identity,
    })
}

pub fn is_delivery_tag_namespace(tag: &str) -> bool {
    tag.starts_with(DELIVERY_TAG_PREFIX)
}

fn parse_tag_number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// 現在時刻が属する再試行スロット。時計が UNIX epoch より前なら 0。
pub fn retry_slot(now: SystemTime) -> u64 {
    let width = QUEUE_RETRY_BACKOFF.as_secs().max(1);
    now.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / width
}

/// 拒否マーカーの dedupe キー。同じ拒否文でも送り主が違えば別件。
pub fn rejection_key(from: u64, rejection: &TalkReject) -> u64 {
    let detail = rejection.detail();
    text_key(b"agent-talk/rejection/v1", from, &detail)
}

fn message_identity(body: &str) -> u64 {
    text_key(b"agent-talk/message/v1", 0, body)
}

fn numeric_key(domain: &[u8], values: &[u64]) -> u64 {
    let mut hash = crate::history::Fnv1a64::default();
    hash.update(domain);
    for value in values {
        hash.update(&value.to_le_bytes());
    }
    hash.finish()
}

fn text_key(domain: &[u8], prefix: u64, text: &str) -> u64 {
    let mut hash = crate::history::Fnv1a64::default();
    hash.update(domain);
    hash.update(&prefix.to_le_bytes());
    hash.update(&(text.len() as u64).to_le_bytes());
    hash.update(text.as_bytes());
    hash.finish()
}

/// 断った理由 (人へそのまま出す)。**Team 側と同じ型をそのまま使う。**
///
/// 断り文を 2 つ持つと、片方にしか言い分けが入らない。実際にそうなった:
/// Team は「その宛先はあなた自身です」と言えるのに、通常タブは同じ状況で
/// 「居ません」のままで、**同じ状況の説明が食い違っていた**。
///
/// **借りているのは*文面*だけで、*宛先の照合*はこちらが持つ。**
/// Team は ID / 役割の完全一致、通常タブはタブ名の大小無視 + 前方一致で、
/// 規則そのものが違うため `rp::check_message` はそのままでは使えない
/// (規則を Team に合わせると `codex` で `Codex (全自動)` を呼べなくなる)。
///
/// 将来ほんとうに 1 本へまとめるなら、`check_message` から照合を関数引数
/// (`impl Fn(&T) -> bool`) として外へ出し、`(候補一覧, 自分, 照合)` →
/// `Result<Vec<T>, MessageReject>` の 1 本にする。そうすれば**降り方**
/// (`all` → 自分自身 → 表に無い) まで 1 か所になる。いまは
/// `result_parser.rs` を触れないので、降り方だけ [`one`] に写している。
pub type TalkReject = rp::MessageReject;

/// 画面テキストから伝言を取り出して、届け先を決める (**純関数**)。
///
/// `from` は送り主のセッション。`peers` は自分を含む全タブ。
/// `sent` は送り主へ**こちらが送った文面** (あれば) — 指示のエコーを
/// 相手の発言として拾わないために使う (Team と同じ番人)。
pub fn deliveries(
    screen: &str,
    from: u64,
    peers: &[Peer],
    sent: Option<&str>,
) -> (Vec<Delivery>, Vec<TalkReject>) {
    let mut out = Vec::new();
    let mut bad = Vec::new();
    let me = peers.iter().find(|p| p.id == from);
    let sender = me
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "?".to_string());
    for body in rp::extract_blocks(screen, rp::MSG_OPEN, rp::MSG_CLOSE) {
        // **自分の画面に出ている「こちらが送った文面」を発言と読まない。**
        if sent.is_some_and(|s| rp::is_prompt_echo(&body, s, rp::MSG_OPEN, rp::MSG_CLOSE)) {
            continue;
        }
        match one(&body, from, peers, &sender, message_identity(&body)) {
            Ok(mut v) => out.append(&mut v),
            Err(e) => bad.push(e),
        }
    }
    (out, bad)
}

/// 伝言 1 件を届け先へ展開する。
fn one(
    body: &str,
    from: u64,
    peers: &[Peer],
    sender: &str,
    message_identity: u64,
) -> Result<Vec<Delivery>, TalkReject> {
    // **読み方は Team と同じ 1 つ** (`rp::read_message`)。ここに 2 つめの
    // パーサを置くと、手書き JSON の綴り間違いを拾えるのが片方だけになる。
    let doc = rp::read_message(body)?;
    let to = doc.to.as_str();
    if to.is_empty() {
        return Err(TalkReject::NoTarget);
    }
    let text = doc.text.clone();
    if text.is_empty() {
        return Err(TalkReject::Empty);
    }
    // **照合はタブ固有** (大小無視 + 前方一致)。人が打つ名前は揺れるので、
    // 揺れを吸収するのはこちら側の仕事 — Team の ID / 役割は完全一致で、
    // ここを Team に合わせると `codex` で `Codex (全自動)` を呼べなくなる。
    let want = to.to_lowercase();
    let hits = |p: &Peer| {
        let n = p.name.to_lowercase();
        n == want || n.starts_with(&want) || want.starts_with(&n)
    };
    let all = to.eq_ignore_ascii_case("all");
    let others = || peers.iter().filter(|p| p.id != from);
    let targets: Vec<&Peer> = if all {
        others().collect()
    } else {
        others().filter(|p| hits(p)).collect()
    };
    if targets.is_empty() {
        // **「居ません」は最後の枝。** 降り方は Team の `check_message` と
        // 同じ順 (`all` → 自分自身 → 表に無い)。順番を変えると、相手は
        // 居るのに「居ません」と言うことになり、エージェントは綴りを疑って
        // 同じ宛先を書き直す (Team 側が実機で 2 回踏んだ)。
        if all {
            // 居ないのは相手ではなく「あなた以外のタブ」。
            return Err(TalkReject::NoOtherAgents);
        }
        if peers.iter().any(|p| p.id == from && hits(p)) {
            // **自分自身。** 相手が居ないのではなく、宛先が自分だから届かない。
            // 判定には**相手を探したのと同じ照合**を使う (別の規則で見ると、
            // 相手には当たらないのに自分にも当たらない宛先が出る)。
            return Err(TalkReject::SelfTarget(to.to_string()));
        }
        // 表に無い宛先は今までどおり断る (捏造を通さない)。書けた宛先も
        // 一緒に返す — 断るだけだと、送り主は同じ綴りを書き直すしかない。
        return Err(TalkReject::UnknownTarget {
            to: to.to_string(),
            known: others().map(|p| p.name.clone()).collect(),
        });
    }
    Ok(targets
        .into_iter()
        .map(|p| Delivery {
            from,
            to: p.id,
            message_identity,
            // **誰からかを本文に残す。** 相手の端末には差出人が出ないので、
            // 書かないと「誰かから何か来た」になる。
            text: format!("[Zaivern] {sender} からの伝言:\n{text}"),
        })
        .collect())
}

/// エージェントへ「伝言の使い方」を教える文面。
///
/// **知らなければ一生使わない。** 通常タブには Team のような指示文が無いので、
/// 人が 1 回これを送って教える (パレットの `agent_talk.teach`)。
pub fn how_to(peers: &[Peer], me: u64) -> String {
    let list: String = peers
        .iter()
        .filter(|p| p.id != me)
        .map(|p| format!("* `{}`\n", p.name))
        .collect();
    format!(
        "これから、同じ画面に居る他のエージェントへ**直接**伝言を送れます。\n\n\
         ## いま居る相手\n{list}\n\
         区切りが付いたときや、相手が待っていることが分かったときは、\
         次の形で出してください (Zaivern が相手の端末へ届けます)。\n\n\
         {howto}",
        howto = rp::message_howto("<上の名前、全員なら all>"),
    )
}

#[cfg(test)]
mod wiring {

    /// **通常タブでもエージェント同士が伝言できる。**
    ///
    /// Team Run の中では前から動いていたが、普通に並べているタブには経路が
    /// 無かった。読み取りも配達も**同じ部品**を使い回す (第 2 の経路を作らない)。
    #[test]
    fn 通常タブの伝言は既存の部品を使い回す() {
        let s = include_str!("app/orchestrate.rs").replace("\r\n", "\n");
        let body = s
            .split("fn deliver_agent_talk")
            .nth(1)
            .and_then(|t| t.split("\n    /// ").next())
            .expect("配達の関数がある");
        // 取り出しは Team と同じ (`agent_talk` 越しに `result_parser` を通る)。
        assert!(body.contains("agent_talk::"), "取り出しを自前で書いている");
        // 配達は人が送るのと同じ経路。PTY へ直接書かない。
        assert!(
            body.contains("submit::Job::user"),
            "既存の送信経路を通っていない"
        );
        assert!(!body.contains("write_bytes"), "PTY へ直接書いている");
        let queued = body
            .find("if self.queue_submit(job)")
            .expect("submit キューの受理を確認していない");
        let in_flight = body
            .find("self.talk_once(in_flight_key)")
            .expect("queue 中の重複投入を防いでいない");
        assert!(queued < in_flight, "queue 受理前に配送中にしている");
        assert!(
            body.contains("job.tag = Some(delivery_tag)"),
            "outcome の戻し先タグが無い"
        );
        assert!(
            !body.contains("self.talk_once(delivered_key)"),
            "submit の実配送 ACK 前に配達済みにしている"
        );
        assert!(
            body.contains("attempt_key(retry_slot)") && body.contains("queue_failure_notice_key"),
            "queue 拒否を毎フレーム連打するか、失敗を黙殺している"
        );
        let outcome = s
            .split("pub(crate) fn note_submit_delivery")
            .nth(1)
            .and_then(|t| t.split("\n    /// ").next())
            .expect("submit outcome の回収口が無い");
        assert!(
            outcome.contains("agent_talk::parse_delivery_tag")
                && outcome.contains("talk_forget(identity.in_flight_key())")
                && outcome.contains("if delivered")
                && outcome.contains("talk_once(identity.delivered_key())"),
            "実配送の成功時だけ delivered へ移す状態遷移が無い"
        );
        assert!(
            outcome.contains("is_delivery_tag_namespace"),
            "壊れた agent-talk タグが Team 側へ流れる"
        );
        // **同じ塊を二度配らない。** 覚え書きは上限つきで、配った伝言と
        // 断った理由の両方が同じ口を通る (片方だけだと際限なく伸びる)。
        assert!(body.contains("talk_once("), "配り済みを覚えていない");
        let once = s
            .split("fn talk_once")
            .nth(1)
            .and_then(|t| t.split("\n    fn ").next())
            .expect("覚え書きの口がある");
        assert!(
            once.contains("talk_seen") && once.contains("TALK_SEEN_CAP"),
            "覚え書きに上限が無い"
        );
        // **毎tick 呼ばれている** (関数だけ作って繋がない、を防ぐ)。
        assert!(
            s.contains("self.deliver_agent_talk();"),
            "配達が毎 tick 呼ばれていない"
        );
        // **見張りが切られていても回る** (伝言は見張りの機能ではない)。
        let i = s.find("self.deliver_agent_talk();").expect("呼び出し");
        let j = s
            .find("if !self.cfg.supervisor.enabled")
            .expect("見張りの門");
        assert!(i < j, "見張りが切られていると伝言も止まる");
    }

    /// **教える入口がある。** 通常タブには Team のような指示文が無いので、
    /// 教えなければエージェントは一生この仕組みを使わない。
    #[test]
    fn 伝言の使い方を教える入口がある() {
        let f = include_str!("features/agent_talk.rs").replace("\r\n", "\n");
        assert!(f.contains("ID_TEACH"), "パレットの項目が無い");
        assert!(f.contains("app.teach_agent_talk()"), "押しても何も起きない");
        let s = include_str!("app/orchestrate.rs").replace("\r\n", "\n");
        assert!(
            s.contains("pub(crate) fn teach_agent_talk"),
            "glue が無い (押せるのに何も起きない)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peers() -> Vec<Peer> {
        vec![
            Peer {
                id: 1,
                name: "Claude Code".into(),
            },
            Peer {
                id: 2,
                name: "Codex".into(),
            },
            Peer {
                id: 3,
                name: "Codex (全自動)".into(),
            },
        ]
    }

    fn msg(to: &str, text: &str) -> String {
        format!(
            "{}\n{{\"to\": \"{to}\", \"text\": \"{text}\"}}\n{}",
            rp::MSG_OPEN,
            rp::MSG_CLOSE
        )
    }

    /// **タブの名前で指せる。** 通常タブには `agent-1` のような ID が無い。
    #[test]
    fn タブの名前で相手を指せる() {
        let (d, bad) = deliveries(&msg("Codex", "テストを回して"), 1, &peers(), None);
        assert!(bad.is_empty(), "{bad:?}");
        // 前方一致で 2 つとも当たる (`Codex` と `Codex (全自動)`)。
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|x| x.text.contains("テストを回して")));
        assert!(
            d[0].text.contains("Claude Code からの伝言"),
            "差出人が本文に無い: {:?}",
            d[0].text
        );
        assert!(d.iter().all(|x| x.from == 1), "差出人 ID が欠けている");
    }

    /// 表示本文が同じでも、送り主・宛先・元ブロックのどれかが
    /// 違えば別の配達として扱う。
    #[test]
    fn 配達キーは送信元_宛先_メッセージを全て含む() {
        let same_names = vec![
            Peer {
                id: 1,
                name: "Worker".into(),
            },
            Peer {
                id: 2,
                name: "Target".into(),
            },
            Peer {
                id: 4,
                name: "Worker".into(),
            },
        ];
        let screen = msg("Target", "同じ本文");
        let (from_one, bad) = deliveries(&screen, 1, &same_names, None);
        assert!(bad.is_empty());
        let (from_four, bad) = deliveries(&screen, 4, &same_names, None);
        assert!(bad.is_empty());
        assert_eq!(from_one[0].text, from_four[0].text, "旧キーが衝突する前提");
        assert_ne!(
            from_one[0].delivered_key(),
            from_four[0].delivered_key(),
            "同名の別送信者が衝突している"
        );

        let base = from_one[0].clone();
        let mut other_target = base.clone();
        other_target.to += 1;
        assert_ne!(base.delivered_key(), other_target.delivered_key());

        let mut other_message = base.clone();
        other_message.message_identity ^= 1;
        assert_ne!(base.delivered_key(), other_message.delivered_key());

        let (again, _) = deliveries(&screen, 1, &same_names, None);
        assert_eq!(
            base.delivered_key(),
            again[0].delivered_key(),
            "同じ入力のキーが実行ごとに変わる"
        );
    }

    /// 配達済み・再試行・失敗通知は同じ伝言でも別の名前空間を使う。
    #[test]
    fn 再試行キーは30秒ごとで配達済みと衝突しない() {
        let (items, bad) = deliveries(&msg("Codex", "再試行"), 1, &peers(), None);
        assert!(bad.is_empty());
        let d = &items[0];
        let delivered = d.delivered_key();
        let in_flight = d.in_flight_key();
        let queue_notice = d.queue_failure_notice_key();
        let outcome_notice = d.identity().outcome_failure_notice_key();
        let attempt_0 = d.attempt_key(0);
        let attempt_1 = d.attempt_key(1);
        assert_ne!(delivered, queue_notice);
        assert_ne!(delivered, outcome_notice);
        assert_ne!(delivered, in_flight);
        assert_ne!(in_flight, queue_notice);
        assert_ne!(queue_notice, outcome_notice);
        assert_ne!(delivered, attempt_0);
        assert_ne!(queue_notice, attempt_0);
        assert_ne!(outcome_notice, attempt_0);
        assert_ne!(attempt_0, attempt_1);

        assert_eq!(
            retry_slot(SystemTime::UNIX_EPOCH + Duration::from_secs(29)),
            0
        );
        assert_eq!(
            retry_slot(SystemTime::UNIX_EPOCH + Duration::from_secs(30)),
            1
        );
    }

    /// outcome タグは 3 つの ID を欠落・曖昧性なく復元する。
    #[test]
    fn agent_talk配送タグは厳密に往復する() {
        let delivery = Delivery {
            from: u64::MAX - 2,
            to: u64::MAX - 1,
            message_identity: u64::MAX,
            text: "x".into(),
        };
        assert_eq!(
            parse_delivery_tag(&delivery.delivery_tag()),
            Some(delivery.identity())
        );
        for malformed in [
            "agent-talk:",
            "agent-talk:1:2",
            "agent-talk:1:2:",
            "agent-talk::2:3",
            "agent-talk:+1:2:3",
            "agent-talk: 1:2:3",
            "agent-talk:1:2:3:4",
            "agent-talk:1:2:18446744073709551616",
            "team:1:2:3",
        ] {
            assert_eq!(parse_delivery_tag(malformed), None, "{malformed}");
        }
        assert!(is_delivery_tag_namespace("agent-talk:broken"));
    }

    /// 拒否マーカーの重複排除にも送信元を含める。
    #[test]
    fn 拒否キーは同じ理由の別送信者を潰さない() {
        let rejection = TalkReject::NoOtherAgents;
        assert_eq!(rejection_key(1, &rejection), rejection_key(1, &rejection));
        assert_ne!(rejection_key(1, &rejection), rejection_key(2, &rejection));
    }

    /// **自分自身へは届けない。** 無限に往復しうる。
    #[test]
    fn 自分自身へは届けない() {
        let (d, _) = deliveries(&msg("all", "できた"), 2, &peers(), None);
        let ids: Vec<u64> = d.iter().map(|x| x.to).collect();
        assert_eq!(ids, vec![1, 3], "自分 (2) が入っている");
    }

    /// **居ない相手は断る。** 届いた気にさせない (捏造を通さない)。
    #[test]
    fn 居ない相手は断る() {
        let (d, bad) = deliveries(&msg("だれか", "やあ"), 1, &peers(), None);
        assert!(d.is_empty());
        assert!(
            matches!(bad.as_slice(), [TalkReject::UnknownTarget { .. }]),
            "{bad:?}"
        );
        assert!(bad[0].detail().contains("だれか"));
        // **書けた宛先も返す。** 断るだけだと同じ綴りを書き直すしかない。
        assert!(bad[0].detail().contains("Codex"), "{}", bad[0].detail());
    }

    /// **自分の名前を宛先に書いても「居ません」と言わない。**
    ///
    /// 実機で Team 側が踏んだのと同じ形 (役割 `tester` の担当が
    /// `"to": "tester"` と書いた)。相手は居るのに「居ません」と返すと、
    /// エージェントは綴りを疑って**同じ宛先を書き直す** — 2 回繰り返した。
    #[test]
    fn 自分の名前を宛先に書いたら自分自身だと言う() {
        // `Claude Code` から `Claude Code` 宛て。他のタブには 1 つも当たらない。
        let (d, bad) = deliveries(&msg("Claude Code", "頼む"), 1, &peers(), None);
        assert!(d.is_empty(), "自分へ届けている: {d:?}");
        assert!(
            matches!(bad.as_slice(), [TalkReject::SelfTarget(t)] if t == "Claude Code"),
            "{bad:?}"
        );
        // **文面も Team と同じ 1 つ。** ここで別の文を作ると食い違う。
        assert_eq!(
            bad[0].detail(),
            rp::MessageReject::SelfTarget("Claude Code".to_string()).detail()
        );
        assert_ne!(
            bad[0].detail(),
            rp::MessageReject::UnknownTarget {
                to: "Claude Code".to_string(),
                known: vec!["Codex".to_string(), "Codex (全自動)".to_string()],
            }
            .detail(),
            "「居ません」のままになっている"
        );
    }

    /// **1 つしかタブが無いのに `all` と書いたとき、宛先を疑わせない。**
    /// 居ないのは相手ではなく「あなた以外のタブ」なので、`all` の綴りを
    /// 直させても永久に直らない。
    #[test]
    fn 自分しか居ないallは宛先のせいにしない() {
        let only = vec![Peer {
            id: 1,
            name: "Claude Code".into(),
        }];
        let (d, bad) = deliveries(&msg("all", "できた"), 1, &only, None);
        assert!(d.is_empty(), "{d:?}");
        assert!(
            matches!(bad.as_slice(), [TalkReject::NoOtherAgents]),
            "{bad:?}"
        );
        assert_eq!(bad[0].detail(), rp::MessageReject::NoOtherAgents.detail());
        assert_ne!(
            bad[0].detail(),
            rp::MessageReject::UnknownTarget {
                to: "all".to_string(),
                known: Vec::new(),
            }
            .detail(),
            "all の綴りを疑わせている"
        );
    }

    /// **断り文はこちらで作り直さない。** 2 つ持つと、片方にしか
    /// 言い分けが入らず、同じ状況で説明が食い違う (実際にそうなった)。
    #[test]
    fn 断り文をこちらで作り直していない() {
        let src = include_str!("agent_talk.rs").replace("\r\n", "\n");
        // 見るのは**テストより手前だけ**。全体を見ると、この検査自身が
        // 書いた文字列を拾って空回りする (わざと壊しても緑になる)。
        let head = src.split("#[cfg(test)]").next().expect("本体がある");
        assert!(
            !head.contains("fn detail"),
            "断り文をこちらで作り直している (Team と食い違う)"
        );
        assert!(
            head.contains("pub type TalkReject = rp::MessageReject;"),
            "断り理由の型が Team と別になっている"
        );
    }

    /// **本文に生の改行が入っていても届く** (Team と同じ読み取りを通す)。
    #[test]
    fn 生の改行が入っていても届く() {
        let body = format!(
            "{}\n{{\"to\": \"Codex\", \"text\": \"1 行目\n2 行目\"}}\n{}",
            rp::MSG_OPEN,
            rp::MSG_CLOSE
        );
        let (d, bad) = deliveries(&body, 1, &peers(), None);
        assert!(bad.is_empty(), "{bad:?}");
        assert!(d[0].text.contains("2 行目"));
    }

    /// **こちらが送った使い方の説明を、相手の発言として拾わない。**
    /// 説明にはひな型がマーカーごと載っているので、素直に拾うと
    /// `"to": "<上の名前…>"` を宛先として扱ってしまう。
    #[test]
    fn 使い方の説明を発言として拾わない() {
        let teach = how_to(&peers(), 1);
        let (d, bad) = deliveries(&teach, 1, &peers(), Some(&teach));
        assert!(d.is_empty(), "ひな型を伝言として届けている: {d:?}");
        assert!(bad.is_empty(), "ひな型を断りとして数えている: {bad:?}");
    }
}
