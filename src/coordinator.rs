//! エージェント調停レイヤ — セッション間の連絡と、停滞タスクの再割り当て。
//!
//! ## 何を解決するか
//!
//! 1. **連絡** — 走っている CLI エージェント同士 / 監督役 / ユーザーの間で
//!    メッセージをやり取りする。ただし CLI エージェントへの入り口は PTY の
//!    標準入力しか無く、**生成中に書き込むと入力が壊れる**。だからメッセージは
//!    必ずキューに積み、相手が「注入して安全な状態」のときだけ配達する。
//! 2. **再割り当て** — タスク担当が固まった / 死んだときに別のエージェントへ
//!    引き継ぐ。ただし**前任者が確実に停止したと確認できるまで引き渡さない**。
//!    2 つのエージェントが同じファイルを同時に編集すると成果物が壊れるため。
//!
//! ## 設計の方針
//!
//! - 調停の中核 (`Coordinator`) は **他モジュールへ一切依存しない**。
//!   セッションの状態や承認モードは呼び出し側が自前の型へ変換して渡す。
//!   監督レイヤ(supervisor)とも型を共有しないので、どちらが先に出来ても壊れない。
//!   例外は末尾のクォータ監視 (`quota` 子モジュール + [`QuotaWatch`]) だけで、
//!   ここは i18n とレート制限検知 (terminal) を再利用する。
//! - **スレッドを使わない**。全て同期的な純メモリ操作で、1 フレーム分の呼び出しは
//!   セッション数・キュー長に対して線形かつ上限付き。UI スレッドを塞がない。
//! - **メモリは全て有界**。リングバッファ・窓の刈り取り・履歴の上限を徹底する。
//! - **黙って捨てない**。落としたメッセージは必ず理由付きで記録・計数する。

// 公開 API 一式を先に用意し、app.rs 側の配線は後から行うため、
// 未使用の警告を抑える(keybinds.rs / editor_ops.rs と同じ扱い)。
#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

// ── 上限値(既定) ───────────────────────────────────────────────────────

/// 転送の最大ホップ数。これを超えたメッセージは捨てる(ループ止め)。
pub const DEFAULT_MAX_HOPS: u8 = 4;
/// レート制限の窓幅。
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(10);
/// 同一 (送信元, 宛先) ペアが窓内に送れる本数。
pub const DEFAULT_PAIR_LIMIT: u32 = 8;
/// 全体で窓内に送れる本数。
pub const DEFAULT_GLOBAL_LIMIT: u32 = 40;
/// ブロードキャストが窓内に送れる本数(直接送信よりきつく絞る)。
pub const DEFAULT_BROADCAST_LIMIT: u32 = 2;
/// ピンポン判定の窓幅。
pub const DEFAULT_PINGPONG_WINDOW: Duration = Duration::from_secs(15);
/// ピンポン判定のしきい値(2 者間の往復本数の合計)。
pub const DEFAULT_PINGPONG_LIMIT: u32 = 6;
/// メールボックス 1 個あたりの保持本数(超えたら古いものから捨てる)。
pub const DEFAULT_MAILBOX_CAP: usize = 64;
/// 破棄ログの保持件数。
pub const DEFAULT_DROP_LOG_CAP: usize = 128;
/// タスク再試行の既定上限。使い切ったら NeedsUser。
pub const DEFAULT_MAX_ATTEMPTS: u8 = 3;
/// タスク履歴の保持件数。
pub const HISTORY_CAP: usize = 64;
/// 引き継ぎコンテキストの保持件数。
pub const CONTEXT_CAP: usize = 32;
/// 引き継ぎコンテキスト 1 件の最大文字数。
pub const CONTEXT_ITEM_MAX: usize = 500;
/// PTY へ注入する本文の最大文字数。
pub const INJECT_BODY_MAX: usize = 600;
/// submit キュー自体へ積めなかったときの再試行間隔。
pub const DELIVERY_QUEUE_RETRY_BACKOFF: Duration = Duration::from_secs(30);
/// submit キュー拒否の再試行上限。上限後は人へ返す。
pub const DELIVERY_QUEUE_RETRY_MAX: u8 = 4;
/// 追跡するペア数の上限(これを超えたら空の窓を掃除する)。
const PAIR_TRACK_CAP: usize = 256;

/// PTY へ注入したメッセージに付ける目印。
///
/// 端末を人間が見たときに「これは自分が打ったのではない」と一目で分かるようにする。
pub const INJECT_PREFIX: &str = "[ZAI-AGENT]";

// ── 宛先とメッセージ ──────────────────────────────────────────────────

/// セッション識別子。`terminal::Session::id` と同じ値を渡す想定。
pub type SessionId = u64;
/// タスク識別子。
pub type TaskId = u64;

/// メッセージの送信元 / 宛先。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Endpoint {
    /// 実行中の CLI エージェントセッション。
    Session(SessionId),
    /// 監督レイヤ(異常検知など)。
    Supervisor,
    /// 人間のユーザー。ここへ届いたものは UI で必ず見せる。
    User,
    /// 全セッション宛。
    Broadcast,
}

/// メッセージの種別。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgKind {
    /// 依頼。
    Request,
    /// 返答。
    Reply,
    /// タスクの引き継ぎ。
    Handoff,
    /// 状況報告。
    Status,
    /// 質問。
    Question,
    /// 人間へのエスカレーション。
    Escalation,
}

impl MsgKind {
    /// 端末へ注入するときの日本語ラベル。
    pub fn label(self) -> &'static str {
        match self {
            MsgKind::Request => "依頼",
            MsgKind::Reply => "返答",
            MsgKind::Handoff => "引き継ぎ",
            MsgKind::Status => "状況",
            MsgKind::Question => "質問",
            MsgKind::Escalation => "エスカレーション",
        }
    }
}

/// エージェント間メッセージ 1 通。
#[derive(Clone, Debug)]
pub struct AgentMessage {
    /// 連番 ID。`Coordinator::enqueue` が採番する(投入前は 0)。
    pub id: u64,
    pub from: Endpoint,
    pub to: Endpoint,
    pub kind: MsgKind,
    pub body: String,
    /// 生成時刻。レート制限の窓もこの時刻を基準に判定するため、
    /// テストからは任意の時刻を差し込める。
    pub at: Instant,
    /// 転送するたびに 1 増える。`max_hops` を超えたら捨てる。
    pub hops: u8,
}

impl AgentMessage {
    /// 「いま」の時刻でメッセージを作る。
    pub fn new(from: Endpoint, to: Endpoint, kind: MsgKind, body: impl Into<String>) -> Self {
        Self {
            id: 0,
            from,
            to,
            kind,
            body: body.into(),
            at: Instant::now(),
            hops: 0,
        }
    }

    /// 時刻を差し替える(テストと、まとめ処理で時刻を揃えたいとき用)。
    pub fn at(mut self, at: Instant) -> Self {
        self.at = at;
        self
    }
}

// ── セッション状態と「注入して安全か」の判定 ─────────────────────────────

/// 調停レイヤから見たセッションの状態。
///
/// `terminal::Session` の生の状態からの変換は呼び出し側の責任。
/// 判断がつかないときは必ず `Unknown` を渡すこと(既定で配達しない)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// プロンプトで待機中。注入して安全。
    Idle,
    /// 入力待ち(プロンプトが出て人のターンになっている)。注入して安全。
    AwaitingInput,
    /// 生成中 / 作業中。**注入すると入力が壊れる**。
    Working,
    /// 承認プロンプト待ち。**絶対に注入しない**
    /// (本文がそのまま承認の返事として解釈されてしまう)。
    WaitingApproval,
    /// 無反応。内部状態が読めないので注入しない。
    Stalled,
    /// 終了済み。
    Exited,
    /// 不明。既定で注入しない。
    Unknown,
}

/// 注入して安全な状態かどうか。
///
/// 安全な集合は `Idle` と `AwaitingInput` の 2 つだけ。それ以外は全て不可で、
/// 特に `WaitingApproval` と `Unknown` は明示的に不可とする。
pub fn deliverable(state: SessionState) -> bool {
    match state {
        SessionState::Idle | SessionState::AwaitingInput => true,
        SessionState::Working
        | SessionState::WaitingApproval
        | SessionState::Stalled
        | SessionState::Exited
        | SessionState::Unknown => false,
    }
}

/// タスクを割り当ててよい状態か(忙しくても割り当て自体は可能)。
/// **配ってよい状態か。ここが唯一の決め所。**
///
/// スケジューラ ([`crate::features::team::imp::scheduler::Candidate::free`]) も
/// この関数を通す。2 つ持つと、**スケジューラが提案して調停層が断る**組み合わせが
/// 生まれ、毎 tick 「割り当てを見送りました」が記録される (実測で台帳が
/// 500 件のそれだけで埋まり、他の記録が全部押し出された)。
pub(crate) fn assignable(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::Idle | SessionState::AwaitingInput | SessionState::Working
    )
}

/// 空いている(= 忙しくない)なら 0、忙しいなら 1。割り当ての優先順位に使う。
fn busy_rank(state: SessionState) -> u8 {
    match state {
        SessionState::Idle | SessionState::AwaitingInput => 0,
        _ => 1,
    }
}

// ── 承認モード ───────────────────────────────────────────────────────

/// 承認モード。`agents::Approval` と 1:1 で対応する写し。
///
/// 依存を断つためにこちら側で持つ。変換は呼び出し側で行う
/// (`Approval::Ask => PermissionMode::Ask` など)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionMode {
    Ask,
    Auto,
    Agent,
}

/// 破壊的な操作の提案。実行の可否は承認モードのゲートを通す。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Proposal {
    /// セッションを停止する。作業中の内容を捨てる可能性があるため破壊的。
    StopSession {
        session: SessionId,
        task: TaskId,
        /// 日本語の理由(UI にそのまま出せる)。
        reason: String,
    },
}

/// 提案をどう扱うか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposalGate {
    /// そのまま実行してよい。
    AutoApproved,
    /// ユーザーの明示的な確認が要る。
    NeedsUserConfirm,
}

/// 承認モードから提案の扱いを決める。
///
/// セッション停止は作業中の成果を捨てうるので、自動で通すのは `Auto` のときだけ。
/// `Agent`(プリセット任せ)は調停レイヤ側の意味が定義できないため、
/// 安全側に倒してユーザー確認を要求する。
pub fn gate_for(mode: PermissionMode) -> ProposalGate {
    match mode {
        PermissionMode::Auto => ProposalGate::AutoApproved,
        PermissionMode::Ask | PermissionMode::Agent => ProposalGate::NeedsUserConfirm,
    }
}

// ── 破棄理由 ─────────────────────────────────────────────────────────

/// メッセージを捨てた理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    /// ホップ数超過(転送ループ)。
    HopLimit { hops: u8 },
    /// (送信元, 宛先) ペアのレート制限。
    RateLimitPair,
    /// 全体のレート制限。
    RateLimitGlobal,
    /// ブロードキャストのレート制限。
    RateLimitBroadcast,
    /// ピンポン(2 者間の往復)を検出して抑制。
    PingPong,
    /// メールボックス溢れ(古いものを押し出した)。
    MailboxOverflow,
    /// 宛先セッションが登録されていない。
    UnknownTarget,
    /// 自分宛(送信元と宛先が同じ)。
    SelfAddressed,
}

/// 破棄理由の種別だけを取り出したもの(計数用のキー)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DropKind {
    HopLimit,
    RateLimitPair,
    RateLimitGlobal,
    RateLimitBroadcast,
    PingPong,
    MailboxOverflow,
    UnknownTarget,
    SelfAddressed,
}

impl DropReason {
    pub fn kind(self) -> DropKind {
        match self {
            DropReason::HopLimit { .. } => DropKind::HopLimit,
            DropReason::RateLimitPair => DropKind::RateLimitPair,
            DropReason::RateLimitGlobal => DropKind::RateLimitGlobal,
            DropReason::RateLimitBroadcast => DropKind::RateLimitBroadcast,
            DropReason::PingPong => DropKind::PingPong,
            DropReason::MailboxOverflow => DropKind::MailboxOverflow,
            DropReason::UnknownTarget => DropKind::UnknownTarget,
            DropReason::SelfAddressed => DropKind::SelfAddressed,
        }
    }

    /// UI に出す日本語の説明。
    pub fn label(self) -> String {
        match self {
            DropReason::HopLimit { hops } => format!("転送回数の上限超過 ({hops} ホップ)"),
            DropReason::RateLimitPair => "同一相手への送信が多すぎる".into(),
            DropReason::RateLimitGlobal => "全体の送信量が多すぎる".into(),
            DropReason::RateLimitBroadcast => "一斉送信が多すぎる".into(),
            DropReason::PingPong => "2 者間の往復を検出したため抑制".into(),
            DropReason::MailboxOverflow => "受信箱が満杯のため古いものを破棄".into(),
            DropReason::UnknownTarget => "宛先セッションが存在しない".into(),
            DropReason::SelfAddressed => "自分宛のため破棄".into(),
        }
    }
}

/// 破棄の記録 1 件。
#[derive(Clone, Debug)]
pub struct DropRecord {
    pub at: Instant,
    pub msg_id: u64,
    pub from: Endpoint,
    pub to: Endpoint,
    pub reason: DropReason,
}

/// 送信の結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// 受信箱に積んだ(まだ配達はしていない)。
    Queued { id: u64 },
    /// 一斉送信で n 個の受信箱へ積んだ。
    Broadcast { id: u64, delivered_to: usize },
    /// 捨てた。理由付き。
    Dropped { reason: DropReason },
}

// ── メールボックス(有界リングバッファ) ────────────────────────────────

/// セッション 1 つ分の受信箱。上限に達したら**古いものから捨てる**。
#[derive(Debug)]
pub struct Mailbox {
    queue: VecDeque<AgentMessage>,
    cap: usize,
    /// 溢れて捨てた累計本数。
    dropped_oldest: u32,
    /// 配達済みの累計本数。
    delivered: u32,
}

impl Mailbox {
    fn new(cap: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            cap: cap.max(1),
            dropped_oldest: 0,
            delivered: 0,
        }
    }

    /// 末尾へ積む。溢れたら押し出された 1 通を返す。
    fn push(&mut self, msg: AgentMessage) -> Option<AgentMessage> {
        let evicted = if self.queue.len() >= self.cap {
            self.dropped_oldest = self.dropped_oldest.saturating_add(1);
            self.queue.pop_front()
        } else {
            None
        };
        self.queue.push_back(msg);
        evicted
    }

    /// ACK 待ちの先頭だけは保護して積む。
    ///
    /// 通常は古い未配達メッセージを押し出す。`cap == 1` で先頭が
    /// 予約中のときだけ、ACK まで最大 `cap + 1` 件を保持する。
    /// その後の投函は予約中でない最古の 1 件と入れ替えるので、
    /// メモリは常に有界である。
    fn push_preserving_front(
        &mut self,
        msg: AgentMessage,
        protected_msg_id: Option<u64>,
    ) -> Option<AgentMessage> {
        let front_is_protected = self
            .front()
            .is_some_and(|front| Some(front.id) == protected_msg_id);
        if !front_is_protected || self.queue.len() < self.cap {
            return self.push(msg);
        }

        let evicted = if self.queue.len() >= 2 {
            self.dropped_oldest = self.dropped_oldest.saturating_add(1);
            self.queue.remove(1)
        } else {
            // cap == 1: 予約中の 1 件を消すより、1 件だけ一時的に増やす。
            None
        };
        self.queue.push_back(msg);
        evicted
    }

    fn pop(&mut self) -> Option<AgentMessage> {
        let m = self.queue.pop_front();
        if m.is_some() {
            self.delivered = self.delivered.saturating_add(1);
        }
        m
    }

    fn front(&self) -> Option<&AgentMessage> {
        self.queue.front()
    }

    /// 予約した先頭メッセージを、ID が一致するときだけ完了する。
    /// 成功 ACK のときだけ配達済み累計を増やす。
    fn finish(&mut self, msg_id: u64, delivered: bool) -> bool {
        if self.front().is_none_or(|m| m.id != msg_id) {
            return false;
        }
        self.queue.pop_front();
        if delivered {
            self.delivered = self.delivered.saturating_add(1);
        }
        true
    }

    /// 溜まっている本数。
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// 溢れて捨てた累計本数。
    pub fn dropped_oldest(&self) -> u32 {
        self.dropped_oldest
    }

    /// 配達した累計本数。
    pub fn delivered(&self) -> u32 {
        self.delivered
    }

    /// 中身を覗く(UI 表示用)。
    pub fn iter(&self) -> impl Iterator<Item = &AgentMessage> {
        self.queue.iter()
    }
}

// ── 配達 ─────────────────────────────────────────────────────────────

/// 1 通分の配達指示。呼び出し側が `Session::send_text(&text)` へ流す。
#[derive(Clone, Debug)]
pub struct Delivery {
    pub session: SessionId,
    pub msg_id: u64,
    /// 共通の submit キューへ渡す本文。確定キーは含まない。
    pub text: String,
}

/// 本文を 1 行へ潰し、制御文字を除いて長さを切り詰める。
///
/// CLI エージェントの入力は「1 行 + Enter」で 1 ターン。本文中に改行があると
/// 途中で送信されてしまうため、改行は区切り記号へ置き換える。
fn sanitize_body(body: &str) -> String {
    let mut out = String::with_capacity(body.len().min(INJECT_BODY_MAX) + 8);
    let mut pending_break = false;
    // 文字数は自前で数える (毎文字 out.chars().count() を呼ぶと O(n×上限))
    let mut count = 0usize;
    for ch in body.chars() {
        if count >= INJECT_BODY_MAX {
            out.push('…');
            break;
        }
        if ch == '\n' || ch == '\r' {
            pending_break = true;
            continue;
        }
        if ch.is_control() {
            continue;
        }
        if pending_break {
            if !out.is_empty() {
                out.push_str(" / ");
                count += 3;
            }
            pending_break = false;
        }
        out.push(ch);
        count += 1;
    }
    out
}

/// 送信元を人が読める短い表記にする。
fn endpoint_label(e: Endpoint) -> String {
    match e {
        Endpoint::Session(id) => format!("session:{id}"),
        Endpoint::Supervisor => "supervisor".into(),
        Endpoint::User => "user".into(),
        Endpoint::Broadcast => "broadcast".into(),
    }
}

/// PTY へ注入する 1 行を組み立てる。
///
/// 先頭に [`INJECT_PREFIX`] を置くので、端末を見ている人間には
/// 「機械が入れた行」だと分かる。末尾の `\r` で 1 ターンとして確定させる。
pub fn format_injection(msg: &AgentMessage) -> String {
    let mut text = format_injection_body(msg);
    text.push('\r');
    text
}

/// 共通の submit キューへ渡す、確定キーを含まない注入本文。
fn format_injection_body(msg: &AgentMessage) -> String {
    format!(
        "{} #{} {}から({}): {}",
        INJECT_PREFIX,
        msg.id,
        endpoint_label(msg.from),
        msg.kind.label(),
        sanitize_body(&msg.body)
    )
}

// ── 発信マーカー ─────────────────────────────────────────────────────

/// エージェントが「別のエージェントへ送りたい」ときに **自分で書く** 行の接頭辞。
///
/// 受信側の [`INJECT_PREFIX`] と対になる。書式は
/// `[ZAI-TO:<宛先>] <本文>` の 1 行で、`<宛先>` はセッション名か
/// [`OUTBOUND_ALL`]。LLM に解釈させず、この決め打ちの形だけを見る。
pub const OUTBOUND_PREFIX: &str = "[ZAI-TO:";

/// 一斉送信を表す宛先ラベル。
pub const OUTBOUND_ALL: &str = "ALL";

/// 宛先ラベルの最大文字数。これを超える行は形式不正として捨てる。
const OUTBOUND_TARGET_MAX: usize = 64;

/// 発信マーカー 1 行を解析して `(宛先ラベル, 本文)` を返す **純関数**。
///
/// 解析しないもの(すべて `None`):
///
/// - **行頭以外**にマーカーがある行。プロンプトや引用の中の文字列で
///   誤爆させないため、位置をずらす救済は一切しない。
/// - [`INJECT_PREFIX`] を含む行。注入した行がそのまま画面に出た「こだま」を
///   発信と読むと、送る → 映る → また送る の**無限ループ**になる。
/// - 宛先が空 / `]` が閉じていない / 本文が空。
pub fn parse_outbound(line: &str) -> Option<(String, String)> {
    // 端末は行末を空白で埋める。末尾だけは落とすが、行頭は 1 文字もずらさない。
    let line = line.trim_end_matches([' ', '\t', '\r', '\n']);

    // 注入行のこだま除け。これが唯一のループ止めなので、順序を入れ替えないこと。
    if line.contains(INJECT_PREFIX) {
        return None;
    }

    let rest = line.strip_prefix(OUTBOUND_PREFIX)?;
    let close = rest.find(']')?;
    let target = rest[..close].trim();
    if target.is_empty() || target.chars().count() > OUTBOUND_TARGET_MAX {
        return None;
    }
    // `]` は ASCII なので close + 1 は必ず文字境界。
    let body = sanitize_body(rest[close + 1..].trim());
    if body.is_empty() {
        return None;
    }
    Some((target.to_string(), body))
}

// ── タスク ───────────────────────────────────────────────────────────

/// タスクの状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    /// 未割り当て。
    Pending,
    /// 割り当て済み(まだ動き出していない)。
    Assigned,
    /// 実行中。
    Running,
    /// 停滞。
    Stalled,
    /// 失敗。
    Failed,
    /// 完了。
    Done,
    /// 人手が要る(再試行の上限に達した等)。
    NeedsUser,
}

impl TaskState {
    /// これ以上動かす必要が無い終端状態か。
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskState::Done | TaskState::NeedsUser)
    }
}

/// 再割り当ての理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReassignReason {
    /// 担当が停滞した。
    Stalled,
    /// 担当プロセスが落ちた。
    SessionDied,
    /// 担当が失敗を報告した。
    Failed,
    /// 人手による指示。
    Manual,
}

impl ReassignReason {
    pub fn label(self) -> &'static str {
        match self {
            ReassignReason::Stalled => "停滞",
            ReassignReason::SessionDied => "セッション消滅",
            ReassignReason::Failed => "失敗",
            ReassignReason::Manual => "手動",
        }
    }
}

/// タスクに起きた出来事。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskEvent {
    Created,
    Assigned(SessionId),
    Started(SessionId),
    Stalled(SessionId),
    Failed {
        session: SessionId,
        reason: String,
    },
    Reassigned {
        from: Option<SessionId>,
        to: SessionId,
        reason: ReassignReason,
    },
    /// 引き渡しを拒否した(前任者の停止が未確認 など)。
    HandoverRefused(AssignRefusal),
    /// **担当ファイルの重なりで割り当てを断った**(fail-closed)。
    ///
    /// 「後で気付く衝突」を作らないために、断った事実だけでなく
    /// 誰と・どのパターンで・どう分ければよいかまで履歴へ残す。
    OverlapRefused {
        /// 重なった相手のタスク。
        with: TaskId,
        /// 相手を持っているセッション(未割り当てなら None)。
        holder: Option<SessionId>,
        /// 重なった相手側のパターン。
        pattern: String,
        /// 対処まで書いた文面([`overlap_reason`] の結果)。
        reason: String,
    },
    /// 前任者の停止を確認した。
    PreviousStopped(SessionId),
    /// 引き継ぎ資料を渡した。
    ContextCarried(usize),
    Completed(SessionId),
    /// 人間へ上げた。
    EscalatedToUser(String),
}

/// 割り当てを断った理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignRefusal {
    /// そんなタスクは無い。
    NoSuchTask,
    /// もう終わっている。
    TaskFinished,
    /// **前任者の停止が確認できていない**。
    /// 同じファイルを 2 人が同時に触ると成果物が壊れるため、引き渡さない。
    PreviousHolderNotStopped { previous: SessionId },
    /// 再試行の上限に達した(タスクは NeedsUser になる)。
    AttemptsExhausted { attempts: u8 },
    /// 条件を満たす候補がいない。
    NoEligibleCandidate,
    /// **担当ファイルが他のタスクと重なる**。
    ///
    /// 後勝ちにしない (fail-closed)。詳しい文面は [`TaskEvent::OverlapRefused`]
    /// に残る — ここは `Copy` を保つため ID だけを持つ。
    FileOverlap {
        with: TaskId,
        holder: Option<SessionId>,
    },
}

impl AssignRefusal {
    /// UI に出す日本語の説明。
    pub fn label(self) -> String {
        match self {
            AssignRefusal::NoSuchTask => "該当タスクが無い".into(),
            AssignRefusal::TaskFinished => "タスクは既に終了している".into(),
            AssignRefusal::PreviousHolderNotStopped { previous } => {
                format!("前任 session:{previous} の停止が未確認のため引き渡さない")
            }
            AssignRefusal::AttemptsExhausted { attempts } => {
                format!("再試行の上限に到達 ({attempts} 回) — 人手が必要")
            }
            AssignRefusal::NoEligibleCandidate => "割り当て可能なセッションがいない".into(),
            AssignRefusal::FileOverlap { with, holder } => match holder {
                Some(s) => format!(
                    "タスク #{with} (session:{s} が担当中) と担当ファイルが重なるため割り当てない"
                ),
                None => format!("タスク #{with} と担当ファイルが重なるため割り当てない"),
            },
        }
    }
}

/// 割り当て候補のセッション情報。
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub id: SessionId,
    pub state: SessionState,
    /// 申告している能力(`required_caps` と突き合わせる)。
    pub caps: Vec<String>,
}

impl SessionInfo {
    pub fn new(id: SessionId, state: SessionState, caps: &[&str]) -> Self {
        Self {
            id,
            state,
            caps: caps.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// 作業タスク 1 件。
#[derive(Clone, Debug)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub description: String,
    pub assigned: Option<SessionId>,
    pub state: TaskState,
    pub attempts: u8,
    /// 出来事の履歴(上限 [`HISTORY_CAP`]、古いものから捨てる)。
    pub history: Vec<(Instant, TaskEvent)>,
    pub required_caps: Vec<String>,
    /// **このタスクが触るファイル集合**(スコープルートからの相対パス / `/` 区切り /
    /// glob 可)。空なら「どこを触るか未申告」で、重なり判定の対象にならない。
    ///
    /// 割り当ての瞬間に [`admit`] がここを見て、他のセッションが持っている
    /// タスクと重なるなら**割り当てない**。衝突をマージのときまで持ち越さないため。
    pub files: Vec<String>,

    /// このタスクで失敗した / 停滞したセッション。**二度と割り当てない**。
    failed_by: HashSet<SessionId>,
    /// 前任者の停止が確認できているか。引き渡しの前提条件。
    prev_holder_stopped: bool,
    /// 次の担当へ引き継ぐ材料(上限 [`CONTEXT_CAP`])。
    context: Vec<String>,
    /// 履歴が溢れて捨てた件数。
    history_dropped: u32,
}

impl Task {
    fn record(&mut self, at: Instant, ev: TaskEvent) {
        if self.history.len() >= HISTORY_CAP {
            self.history.remove(0);
            self.history_dropped = self.history_dropped.saturating_add(1);
        }
        self.history.push((at, ev));
    }

    /// このタスクで失敗済みのセッションか。
    pub fn has_failed(&self, s: SessionId) -> bool {
        self.failed_by.contains(&s)
    }

    /// 前任者の停止が確認済みか。
    pub fn previous_stopped(&self) -> bool {
        self.prev_holder_stopped
    }

    /// 引き継ぎ材料。
    pub fn context(&self) -> &[String] {
        &self.context
    }

    /// 履歴が溢れて捨てた件数。
    pub fn history_dropped(&self) -> u32 {
        self.history_dropped
    }

    /// 表示用の短い名前。文面と分割案の両方で使う。
    fn label(&self) -> String {
        if self.title.is_empty() {
            format!("#{}", self.id)
        } else {
            format!("#{} 「{}」", self.id, self.title)
        }
    }
}

// ── ファイルの重なり (割り当て前の fail-closed 判定) ──────────────────
//
// **並列エージェントの価値は、レビュー時の衝突解決コストで相殺される。**
// だから衝突は「後で発見させる」のではなく、配る瞬間に止める。
//
// 重なり判定そのものは書かない。`lease::overlaps` / `lease::split_plan` が
// glob (`**` / `*` / `?`)・末尾スラッシュ・Windows の大小畳み込みまで面倒を
// 見ており、フックと画面と調停で**同じ 1 本**を通すことに意味がある
// (判定が 2 種類あると「フックは止めたのに配ってしまう」が起きる)。

/// 申告されたファイル集合を台帳と同じ正規形へ。空要素と重複は落とす。
///
/// 正規化を 1 箇所に閉じ込めるのは、`src\ui\a.rs` と `./src/ui/a.rs` が
/// **別のパターンとして重なり判定を素通りする**のを防ぐため。
fn normalize_files(files: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in files {
        let p = crate::lease::normalize_path(f);
        if p.is_empty() || out.contains(&p) {
            continue;
        }
        out.push(p);
    }
    out
}

/// 割り当ての可否。**重なったら fail-closed**(後勝ちにしない)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admit {
    /// 誰とも重ならない。割り当ててよい。
    Ok,
    /// 他のセッションが持っているタスクと重なる。
    Overlap {
        /// 重なった相手のタスク。
        with: TaskId,
        /// 相手を持っているセッション。
        holder: Option<SessionId>,
        /// 重なった相手側のパターン。
        pattern: String,
    },
}

impl Admit {
    /// 割り当ててよいか。
    pub fn is_ok(&self) -> bool {
        matches!(self, Admit::Ok)
    }
}

/// そのタスクは今もファイルを押さえているか。
///
/// 押さえているのは「誰かへ割り当て済み」かつ「終端でない」もの。
/// 未割り当て(まだ誰も触っていない)と終端(`Done` / `NeedsUser` = 手が離れた)は
/// 解放済みとみなす。`Failed` / `Stalled` を解放扱いにしないのは、担当が
/// まだ生きて編集している可能性があるため — **迷ったら押さえている側に倒す**。
///
/// **ここが「誰がどの範囲を持っているか」の唯一の判定。** [`admit`] が割り当ての
/// 可否に使い、`team::changeset` が「その変更は他人のものか」に使う。2 つ目の
/// 所有台帳を作らないために公開している — 別に持つと、片方だけを更新した日に
/// 「割り当ては断るのに、変更は他人のものとして見逃す」がありうる。
pub fn occupies(t: &Task) -> bool {
    t.assigned.is_some() && !t.state.is_terminal() && !t.files.is_empty()
}

/// 候補タスクを `to` へ割り当ててよいか。**I/O を持たない純粋関数**。
///
/// - 既に**他のセッション**へ割り当て済みで実行中のタスクと重なったら `Overlap`
/// - **同じセッション**への割り当ては重なってよい(1 人が両方を持つのは安全)
/// - 未割り当て / 完了済み(終端)のタスクとは重なってよい
/// - 候補自身(同じ `id`)は当然除外する — 再割り当てが自分に阻まれない
///
/// 同点のときは**タスク ID の小さい順**で最初の 1 件を返す(決定的)。
pub fn admit(tasks: &[Task], candidate: &Task, to: SessionId) -> Admit {
    if candidate.files.is_empty() {
        return Admit::Ok;
    }
    let mut hit: Option<(TaskId, Option<SessionId>, String)> = None;
    for t in tasks {
        if t.id == candidate.id || t.assigned == Some(to) || !occupies(t) {
            continue;
        }
        for pb in &t.files {
            if candidate
                .files
                .iter()
                .any(|pa| crate::lease::overlaps(pa, pb))
            {
                let better = hit.as_ref().is_none_or(|(id, _, _)| t.id < *id);
                if better {
                    hit = Some((t.id, t.assigned, pb.clone()));
                }
                break;
            }
        }
    }
    match hit {
        Some((with, holder, pattern)) => Admit::Overlap {
            with,
            holder,
            pattern,
        },
        None => Admit::Ok,
    }
}

/// 断ったときの文面。**「拒否しました」だけでは、ユーザーは機能を切るだけ。**
/// 誰と・どのパターンで重なったか、そして**どう分ければ今すぐ進めるか**を出す。
///
/// `Admit::Ok` なら `None`(文面が要らない)。
pub fn overlap_reason(
    tasks: &[Task],
    candidate: &Task,
    to: SessionId,
    a: &Admit,
) -> Option<String> {
    let Admit::Overlap {
        with,
        holder,
        pattern,
    } = a
    else {
        return None;
    };
    let other = tasks.iter().find(|t| t.id == *with);
    let other_name = other
        .map(|t| t.label())
        .unwrap_or_else(|| format!("#{with}"));
    let mine = candidate
        .files
        .iter()
        .find(|pa| crate::lease::overlaps(pa, pattern))
        .cloned()
        .unwrap_or_else(|| pattern.clone());
    let owner = match holder {
        Some(s) => format!("session:{s} が担当中"),
        None => "担当未定".to_string(),
    };

    let (now_ok, serial) = overlap_split(tasks, candidate, to);
    let mut plan = if now_ok.is_empty() {
        "重ならない部分が 1 つも無いので、いま渡せる範囲はありません".to_string()
    } else {
        format!("いま渡せるのは {}", now_ok.join(", "))
    };
    if !serial.is_empty() {
        plan.push_str(&format!(" / 直列にすべきなのは {}", serial.join(", ")));
    }

    let same_session = match holder {
        Some(s) => {
            format!("(3) どうしても同時に進めるなら、両方を session:{s} へ渡す(1 人なら壊れない)")
        }
        None => "(3) 相手の担当を先に決め、どちらが持つかをはっきりさせる".to_string(),
    };

    Some(format!(
        "タスク {mine_name} は割り当てません: {other_name} ({owner}) と担当ファイルが重なります。\n\
         重なり: こちらの「{mine}」 と 相手の「{pattern}」\n\
         同じファイルを 2 人が同時に編集すると、衝突はマージのときまで見えません。\n\
         対処: (1) {other_name} の完了を待つ (2) 担当を分ける — {plan}\n\
         {same_session}",
        mine_name = candidate.label(),
    ))
}

/// **重ならない部分だけ先に割り当てる**分割案。
///
/// 返り値は `(いま候補へ渡してよいパターン, 直列にすべきパターン)`。
/// 既に持っている側を先に並べて [`lease::split_plan`](crate::lease::split_plan)
/// へ渡すので、**先に走っているタスクの担当範囲は決して削られない**。
pub fn overlap_split(
    tasks: &[Task],
    candidate: &Task,
    to: SessionId,
) -> (Vec<String>, Vec<String>) {
    let mut list: Vec<crate::lease::Assignment> = Vec::new();
    for t in tasks {
        if t.id == candidate.id || t.assigned == Some(to) || !occupies(t) {
            continue;
        }
        list.push(crate::lease::Assignment {
            agent: t.label(),
            patterns: t.files.clone(),
        });
    }
    // 候補は最後 — 先着(実行中のタスク)の範囲を奪わせない。
    list.push(crate::lease::Assignment {
        agent: candidate.label(),
        patterns: candidate.files.clone(),
    });
    let (kept, serial) = crate::lease::split_plan(&list);
    let mine = kept.last().map(|a| a.patterns.clone()).unwrap_or_default();
    // `serial` には他人同士の重なりも入り得るので、候補に関係する分だけ出す。
    let serial: Vec<String> = serial
        .into_iter()
        .filter(|p| candidate.files.iter().any(|f| crate::lease::overlaps(f, p)))
        .collect();
    (mine, serial)
}

// ── 上限設定 ─────────────────────────────────────────────────────────

/// 調停レイヤの上限設定。既定値は各定数を参照。
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_hops: u8,
    pub window: Duration,
    pub pair_limit: u32,
    pub global_limit: u32,
    pub broadcast_limit: u32,
    pub pingpong_window: Duration,
    pub pingpong_limit: u32,
    pub mailbox_cap: usize,
    pub drop_log_cap: usize,
    pub max_attempts: u8,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_hops: DEFAULT_MAX_HOPS,
            window: DEFAULT_WINDOW,
            pair_limit: DEFAULT_PAIR_LIMIT,
            global_limit: DEFAULT_GLOBAL_LIMIT,
            broadcast_limit: DEFAULT_BROADCAST_LIMIT,
            pingpong_window: DEFAULT_PINGPONG_WINDOW,
            pingpong_limit: DEFAULT_PINGPONG_LIMIT,
            mailbox_cap: DEFAULT_MAILBOX_CAP,
            drop_log_cap: DEFAULT_DROP_LOG_CAP,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
    }
}

/// 時刻の窓。古いものを刈り取って本数を数えるだけの小さな器。
#[derive(Debug, Default)]
struct Window {
    stamps: VecDeque<Instant>,
}

impl Window {
    /// `now - width` より古い記録を落とす。
    fn prune(&mut self, now: Instant, width: Duration) {
        while let Some(&front) = self.stamps.front() {
            // now より未来の記録は残す(テストで時刻を巻き戻した場合の保険)。
            if now.checked_duration_since(front).is_some_and(|d| d > width) {
                self.stamps.pop_front();
            } else {
                break;
            }
        }
    }

    fn len(&self) -> u32 {
        self.stamps.len() as u32
    }

    fn push(&mut self, now: Instant) {
        self.stamps.push_back(now);
    }
}

/// 2 者間のペアキー(向きを無視する)。
fn unordered(a: Endpoint, b: Endpoint) -> (Endpoint, Endpoint) {
    // Endpoint に順序が無いので、判別用の数値で正規化する。
    let rank = |e: Endpoint| match e {
        Endpoint::Session(id) => (0u8, id),
        Endpoint::Supervisor => (1, 0),
        Endpoint::User => (2, 0),
        Endpoint::Broadcast => (3, 0),
    };
    if rank(a) <= rank(b) {
        (a, b)
    } else {
        (b, a)
    }
}

// ── 本体 ─────────────────────────────────────────────────────────────

/// エージェント調停レイヤ。
///
/// app.rs が 1 つだけ持ち、毎フレーム [`Coordinator::take_deliverable`] を呼ぶ。
pub struct Coordinator {
    limits: Limits,
    next_msg_id: u64,
    next_task_id: TaskId,

    /// セッションごとの受信箱。ここに存在する = 登録済みセッション。
    mailboxes: HashMap<SessionId, Mailbox>,
    /// submit キューへ予約済みの (宛先, メッセージID)。ACK 前には消さない。
    deliveries_in_flight: HashMap<SessionId, u64>,
    /// submit キュー自体へ積めなかったときの再試行時刻。毎フレーム連打を防ぐ。
    delivery_retry_after: HashMap<SessionId, Instant>,
    /// submit キュー拒否回数。同じ先頭 ID にだけ引き継ぐ。
    delivery_retry_attempts: HashMap<SessionId, (u64, u8)>,
    supervisor_inbox: Mailbox,
    user_inbox: Mailbox,

    /// 破棄ログ(有界リング)。
    drop_log: VecDeque<DropRecord>,
    /// 理由ごとの破棄累計。
    drop_counts: HashMap<DropKind, u32>,

    /// (送信元, 宛先) ごとの送信時刻窓。
    pair_windows: HashMap<(Endpoint, Endpoint), Window>,
    /// 向き無視ペアごとの往復時刻窓(ピンポン判定用)。
    pingpong_windows: HashMap<(Endpoint, Endpoint), Window>,
    /// 一度エスカレーション済みのペア(何度も人を呼ばない)。
    pingpong_escalated: HashSet<(Endpoint, Endpoint)>,
    global_window: Window,
    broadcast_window: Window,

    tasks: Vec<Task>,
    /// 直近の割り当て拒否理由。
    last_refusal: Option<AssignRefusal>,
}

impl Default for Coordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl Coordinator {
    pub fn new() -> Self {
        Self::with_limits(Limits::default())
    }

    pub fn with_limits(limits: Limits) -> Self {
        Self {
            next_msg_id: 1,
            next_task_id: 1,
            mailboxes: HashMap::new(),
            deliveries_in_flight: HashMap::new(),
            delivery_retry_after: HashMap::new(),
            delivery_retry_attempts: HashMap::new(),
            supervisor_inbox: Mailbox::new(limits.mailbox_cap),
            user_inbox: Mailbox::new(limits.mailbox_cap),
            drop_log: VecDeque::new(),
            drop_counts: HashMap::new(),
            pair_windows: HashMap::new(),
            pingpong_windows: HashMap::new(),
            pingpong_escalated: HashSet::new(),
            global_window: Window::default(),
            broadcast_window: Window::default(),
            tasks: Vec::new(),
            last_refusal: None,
            limits,
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    // ── セッション登録 ──────────────────────────────────────────────

    /// セッションを登録する(受信箱を用意する)。既存なら何もしない。
    pub fn register_session(&mut self, id: SessionId) {
        let cap = self.limits.mailbox_cap;
        self.mailboxes
            .entry(id)
            .or_insert_with(|| Mailbox::new(cap));
    }

    /// セッションを外す。溜まっていたメッセージは失われるので、
    /// 事前に [`Coordinator::mailbox`] で中身を UI へ出しておくとよい。
    pub fn unregister_session(&mut self, id: SessionId) {
        self.mailboxes.remove(&id);
        self.deliveries_in_flight.remove(&id);
        self.delivery_retry_after.remove(&id);
        self.delivery_retry_attempts.remove(&id);
    }

    /// 登録済みセッションの受信箱。
    pub fn mailbox(&self, id: SessionId) -> Option<&Mailbox> {
        self.mailboxes.get(&id)
    }

    /// 監督レイヤ宛の受信箱。
    pub fn supervisor_inbox(&self) -> &Mailbox {
        &self.supervisor_inbox
    }

    /// ユーザー宛の受信箱(UI に必ず出す)。
    pub fn user_inbox(&self) -> &Mailbox {
        &self.user_inbox
    }

    /// ユーザー宛メッセージを取り出す(取り出したら消える)。
    pub fn take_user_messages(&mut self) -> Vec<AgentMessage> {
        let mut out = Vec::new();
        while let Some(m) = self.user_inbox.pop() {
            out.push(m);
        }
        out
    }

    /// 監督レイヤ宛メッセージを取り出す(取り出したら消える)。
    pub fn take_supervisor_messages(&mut self) -> Vec<AgentMessage> {
        let mut out = Vec::new();
        while let Some(m) = self.supervisor_inbox.pop() {
            out.push(m);
        }
        out
    }

    // ── 破棄ログ ────────────────────────────────────────────────────

    fn record_drop(&mut self, msg: &AgentMessage, reason: DropReason) {
        if self.drop_log.len() >= self.limits.drop_log_cap {
            self.drop_log.pop_front();
        }
        self.drop_log.push_back(DropRecord {
            at: msg.at,
            msg_id: msg.id,
            from: msg.from,
            to: msg.to,
            reason,
        });
        *self.drop_counts.entry(reason.kind()).or_insert(0) += 1;
    }

    /// 破棄ログ(新しいものが後ろ)。
    pub fn drop_log(&self) -> impl Iterator<Item = &DropRecord> {
        self.drop_log.iter()
    }

    /// 理由ごとの破棄累計。
    pub fn drop_count(&self, kind: DropKind) -> u32 {
        self.drop_counts.get(&kind).copied().unwrap_or(0)
    }

    /// 破棄の総数。
    pub fn total_drops(&self) -> u32 {
        self.drop_counts.values().sum()
    }

    // ── 送信 ────────────────────────────────────────────────────────

    /// メッセージを受信箱へ積む。**この時点では PTY へ書かない**。
    ///
    /// 判定の順番は次のとおり。どれかに引っかかったら理由付きで捨てる。
    /// 1. 自分宛
    /// 2. ホップ数超過
    /// 3. ピンポン検出(検出時はユーザーへエスカレーション)
    /// 4. 一斉送信のレート制限 / ペアのレート制限
    /// 5. 全体のレート制限
    /// 6. 宛先の存在確認
    pub fn enqueue(&mut self, mut msg: AgentMessage) -> SendOutcome {
        let now = msg.at;
        if msg.id == 0 {
            msg.id = self.next_msg_id;
            self.next_msg_id += 1;
        }

        // 1) 自分宛は無意味なので捨てる。
        if msg.from == msg.to {
            let r = DropReason::SelfAddressed;
            self.record_drop(&msg, r);
            return SendOutcome::Dropped { reason: r };
        }

        // 2) 転送ループ止め。
        if msg.hops > self.limits.max_hops {
            let r = DropReason::HopLimit { hops: msg.hops };
            self.record_drop(&msg, r);
            return SendOutcome::Dropped { reason: r };
        }

        // 3) ピンポン検出。ユーザー宛/エスカレーションは対象外
        //    (人を呼ぶ経路まで止めてしまうと異常が見えなくなる)。
        if msg.to != Endpoint::User && msg.kind != MsgKind::Escalation {
            if let Some(r) = self.check_pingpong(&msg, now) {
                self.record_drop(&msg, r);
                return SendOutcome::Dropped { reason: r };
            }
        }

        // 4) レート制限。一斉送信は直接送信よりきつく絞る。
        if msg.to == Endpoint::Broadcast {
            self.broadcast_window.prune(now, self.limits.window);
            if self.broadcast_window.len() >= self.limits.broadcast_limit {
                let r = DropReason::RateLimitBroadcast;
                self.record_drop(&msg, r);
                return SendOutcome::Dropped { reason: r };
            }
        } else if msg.to != Endpoint::User && msg.kind != MsgKind::Escalation {
            let key = (msg.from, msg.to);
            self.prune_pair_tracking();
            let w = self.pair_windows.entry(key).or_default();
            w.prune(now, self.limits.window);
            if w.len() >= self.limits.pair_limit {
                let r = DropReason::RateLimitPair;
                self.record_drop(&msg, r);
                return SendOutcome::Dropped { reason: r };
            }
        }

        // 5) 全体のレート制限(ユーザー宛は人が見る唯一の窓口なので免除)。
        if msg.to != Endpoint::User {
            self.global_window.prune(now, self.limits.window);
            if self.global_window.len() >= self.limits.global_limit {
                let r = DropReason::RateLimitGlobal;
                self.record_drop(&msg, r);
                return SendOutcome::Dropped { reason: r };
            }
        }

        // 6) 宛先の存在確認と投函。
        match msg.to {
            Endpoint::Broadcast => {
                let ids: Vec<SessionId> = {
                    let mut v: Vec<SessionId> = self.mailboxes.keys().copied().collect();
                    v.sort_unstable(); // 決定的に配る
                    v
                };
                let mut n = 0usize;
                for id in ids {
                    if Endpoint::Session(id) == msg.from {
                        continue; // 送信元自身へは返さない
                    }
                    let mut copy = msg.clone();
                    copy.to = Endpoint::Session(id);
                    let protected = self.deliveries_in_flight.get(&id).copied();
                    let evicted = self
                        .mailboxes
                        .get_mut(&id)
                        .and_then(|mb| mb.push_preserving_front(copy, protected));
                    if let Some(old) = evicted {
                        self.record_drop(&old, DropReason::MailboxOverflow);
                    }
                    n += 1;
                }
                self.broadcast_window.push(now);
                self.global_window.push(now);
                self.note_pair(&msg, now);
                SendOutcome::Broadcast {
                    id: msg.id,
                    delivered_to: n,
                }
            }
            Endpoint::Session(id) => {
                if !self.mailboxes.contains_key(&id) {
                    let r = DropReason::UnknownTarget;
                    self.record_drop(&msg, r);
                    return SendOutcome::Dropped { reason: r };
                }
                let mid = msg.id;
                self.global_window.push(now);
                self.note_pair(&msg, now);
                let protected = self.deliveries_in_flight.get(&id).copied();
                let evicted = self
                    .mailboxes
                    .get_mut(&id)
                    .and_then(|mb| mb.push_preserving_front(msg, protected));
                if let Some(old) = evicted {
                    self.record_drop(&old, DropReason::MailboxOverflow);
                }
                SendOutcome::Queued { id: mid }
            }
            Endpoint::Supervisor => {
                let mid = msg.id;
                self.global_window.push(now);
                self.note_pair(&msg, now);
                if let Some(old) = self.supervisor_inbox.push(msg) {
                    self.record_drop(&old, DropReason::MailboxOverflow);
                }
                SendOutcome::Queued { id: mid }
            }
            Endpoint::User => {
                let mid = msg.id;
                if let Some(old) = self.user_inbox.push(msg) {
                    self.record_drop(&old, DropReason::MailboxOverflow);
                }
                SendOutcome::Queued { id: mid }
            }
        }
    }

    /// 送信を記録する(レート制限とピンポン判定の窓へ 1 本足す)。
    fn note_pair(&mut self, msg: &AgentMessage, now: Instant) {
        if msg.to != Endpoint::Broadcast {
            self.pair_windows
                .entry((msg.from, msg.to))
                .or_default()
                .push(now);
        }
        if msg.to != Endpoint::User && msg.kind != MsgKind::Escalation {
            self.pingpong_windows
                .entry(unordered(msg.from, msg.to))
                .or_default()
                .push(now);
        }
    }

    /// ピンポン(2 者が窓内で往復しすぎ)を判定する。
    ///
    /// 検出したら抑制し、ユーザーへエスカレーションする(同じペアで 1 回だけ)。
    fn check_pingpong(&mut self, msg: &AgentMessage, now: Instant) -> Option<DropReason> {
        let key = unordered(msg.from, msg.to);
        let width = self.limits.pingpong_window;
        let limit = self.limits.pingpong_limit;
        let w = self.pingpong_windows.entry(key).or_default();
        w.prune(now, width);
        if w.len() < limit {
            return None;
        }
        if self.pingpong_escalated.insert(key) {
            let body = format!(
                "{} と {} の間で往復が {} 回を超えました。以降のやり取りを抑制しています。",
                endpoint_label(key.0),
                endpoint_label(key.1),
                limit
            );
            let esc = AgentMessage {
                id: self.next_msg_id,
                from: Endpoint::Supervisor,
                to: Endpoint::User,
                kind: MsgKind::Escalation,
                body,
                at: now,
                hops: 0,
            };
            self.next_msg_id += 1;
            // 人を呼ぶ経路は制限を通さず直接積む(再帰しない)。
            if let Some(old) = self.user_inbox.push(esc) {
                self.record_drop(&old, DropReason::MailboxOverflow);
            }
        }
        Some(DropReason::PingPong)
    }

    /// 追跡テーブルが増えすぎたら空になった窓を掃除する(メモリ有界化)。
    fn prune_pair_tracking(&mut self) {
        if self.pair_windows.len() > PAIR_TRACK_CAP {
            self.pair_windows.retain(|_, w| w.len() > 0);
        }
        if self.pingpong_windows.len() > PAIR_TRACK_CAP {
            self.pingpong_windows.retain(|_, w| w.len() > 0);
        }
    }

    /// メッセージを別の宛先へ転送する。ホップ数が 1 増える。
    pub fn forward(&mut self, mut msg: AgentMessage, to: Endpoint, now: Instant) -> SendOutcome {
        msg.hops = msg.hops.saturating_add(1);
        msg.from = msg.to;
        msg.to = to;
        msg.at = now;
        msg.id = 0; // 転送は新しい ID を振る
        self.enqueue(msg)
    }

    // ── 配達 ────────────────────────────────────────────────────────

    /// 注入して安全なセッションへ、**1 セッションにつき 1 通だけ**予約する。
    ///
    /// 1 通ずつにするのは、プロンプトへ連続で流し込んで入力を壊さないため。
    /// 続きは次のフレームで配られる。
    ///
    /// `states` は呼び出し側が毎フレーム組み立てる (セッション ID, 状態) の一覧。
    /// ここに載っていないセッションへは配達しない(状態不明 = 配達しない)。
    pub fn take_deliverable(&mut self, states: &[(SessionId, SessionState)]) -> Vec<Delivery> {
        self.take_deliverable_at(states, Instant::now())
    }

    fn take_deliverable_at(
        &mut self,
        states: &[(SessionId, SessionState)],
        now: Instant,
    ) -> Vec<Delivery> {
        let mut out = Vec::new();
        for &(id, st) in states {
            if !deliverable(st) {
                continue;
            }
            if self.deliveries_in_flight.contains_key(&id)
                || self
                    .delivery_retry_after
                    .get(&id)
                    .is_some_and(|at| *at > now)
            {
                continue;
            }
            let Some(msg) = self.mailboxes.get(&id).and_then(Mailbox::front) else {
                continue;
            };
            let msg_id = msg.id;
            let text = format_injection_body(msg);
            self.deliveries_in_flight.insert(id, msg_id);
            out.push(Delivery {
                session: id,
                msg_id,
                text,
            });
        }
        out
    }

    /// submit キューへ積めなかった予約を戻す。
    ///
    /// 上限までは受信箱の本文を保持し、バックオフ後に再試行する。
    /// 上限に達したら未配達のまま受信箱から外し、ユーザーへ 1 回だけ
    /// エスカレーションする。戻り値は「最終失敗になったか」。
    pub fn defer_delivery(&mut self, session: SessionId, msg_id: u64, now: Instant) -> bool {
        if self.deliveries_in_flight.get(&session) != Some(&msg_id) {
            return false;
        }
        self.deliveries_in_flight.remove(&session);

        let attempts = self
            .delivery_retry_attempts
            .entry(session)
            .and_modify(|state| {
                if state.0 == msg_id {
                    state.1 = state.1.saturating_add(1);
                } else {
                    *state = (msg_id, 1);
                }
            })
            .or_insert((msg_id, 1))
            .1;
        if attempts < DELIVERY_QUEUE_RETRY_MAX {
            self.delivery_retry_after
                .insert(session, now + DELIVERY_QUEUE_RETRY_BACKOFF);
            return false;
        }

        self.delivery_retry_after.remove(&session);
        self.delivery_retry_attempts.remove(&session);
        let discarded = self
            .mailboxes
            .get_mut(&session)
            .is_some_and(|mb| mb.finish(msg_id, false));
        if discarded {
            self.escalate(
                format!(
                    "session:{session} への調停メッセージ #{msg_id} を submit キューへ {DELIVERY_QUEUE_RETRY_MAX} 回積めませんでした。コスト上限とセッション状態を確認してください。"
                ),
                now,
            );
        }
        discarded
    }

    /// submit の最終結果を確定する。成功時だけ配達済みに数える。
    ///
    /// 失敗は submit 側が再送と入力欄検証を上限まで行った後にしか来ない。
    /// そこで同じ指示を無限再投入せず、受信箱から外して人へ明示する。
    pub fn finish_delivery(
        &mut self,
        session: SessionId,
        msg_id: u64,
        delivered: bool,
        now: Instant,
    ) -> bool {
        if self.deliveries_in_flight.get(&session) != Some(&msg_id) {
            return false;
        }
        self.deliveries_in_flight.remove(&session);
        self.delivery_retry_after.remove(&session);
        self.delivery_retry_attempts.remove(&session);
        let confirmed = self
            .mailboxes
            .get_mut(&session)
            .is_some_and(|mb| mb.finish(msg_id, delivered));
        if delivered {
            return confirmed;
        }
        if confirmed {
            self.escalate(
                format!(
                    "session:{session} への調停メッセージ #{msg_id} を配達できませんでした。入力状態を確認してください。"
                ),
                now,
            );
        }
        false
    }

    // ── タスク ──────────────────────────────────────────────────────

    /// タスクを登録する(触るファイルは未申告)。
    ///
    /// 触る範囲が分かっているなら [`Coordinator::add_task_with_files`] を使う。
    /// 未申告のタスクは重なり判定の対象にならない — つまり**衝突を止められない**。
    pub fn add_task(
        &mut self,
        title: impl Into<String>,
        description: impl Into<String>,
        required_caps: &[&str],
        now: Instant,
    ) -> TaskId {
        self.add_task_with_files(title, description, required_caps, &[], now)
    }

    /// 担当ファイル付きでタスクを登録する。
    ///
    /// `files` はスコープルートからの相対パス(`/` 区切り、glob 可)。
    /// 正規化は [`lease::normalize_path`](crate::lease::normalize_path) に任せる
    /// ので、`\` 区切りや `./` 付きで渡してもよい。
    pub fn add_task_with_files(
        &mut self,
        title: impl Into<String>,
        description: impl Into<String>,
        required_caps: &[&str],
        files: &[&str],
        now: Instant,
    ) -> TaskId {
        let id = self.next_task_id;
        self.next_task_id += 1;
        let mut t = Task {
            id,
            title: title.into(),
            description: description.into(),
            assigned: None,
            state: TaskState::Pending,
            attempts: 0,
            history: Vec::new(),
            required_caps: required_caps.iter().map(|s| s.to_string()).collect(),
            files: normalize_files(files),
            failed_by: HashSet::new(),
            // まだ誰も持っていないので「前任者は停止済み」と見なす。
            prev_holder_stopped: true,
            context: Vec::new(),
            history_dropped: 0,
        };
        t.record(now, TaskEvent::Created);
        self.tasks.push(t);
        id
    }

    pub fn task(&self, id: TaskId) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// 担当ファイルを後から差し替える(登録時に分かっていなかった場合)。
    ///
    /// 割り当て済みのタスクにも使えるが、**次の割り当てから効く**。
    /// 既に配ってしまったものを遡って止めることはできない。
    pub fn set_task_files(&mut self, task_id: TaskId, files: &[&str]) -> bool {
        let norm = normalize_files(files);
        match self.task_mut(task_id) {
            Some(t) => {
                t.files = norm;
                true
            }
            None => false,
        }
    }

    /// いまこのタスクを `to` へ渡してよいか(**配る前に**確かめる用)。
    ///
    /// 実際の割り当て([`Coordinator::try_assign`])も同じ判定を通るので、
    /// ここが `Ok` を返したものは(候補の条件を満たす限り)通る。
    pub fn admit_task(&self, task_id: TaskId, to: SessionId) -> Admit {
        match self.task(task_id) {
            Some(t) => admit(&self.tasks, t, to),
            None => Admit::Ok,
        }
    }

    /// 断ったときの文面(重ならないなら `None`)。
    pub fn overlap_reason_for(&self, task_id: TaskId, to: SessionId) -> Option<String> {
        let t = self.task(task_id)?;
        overlap_reason(&self.tasks, t, to, &admit(&self.tasks, t, to))
    }

    /// 「重ならない部分だけ先に渡す」分割案。
    pub fn overlap_split_for(&self, task_id: TaskId, to: SessionId) -> (Vec<String>, Vec<String>) {
        match self.task(task_id) {
            Some(t) => overlap_split(&self.tasks, t, to),
            None => (Vec::new(), Vec::new()),
        }
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    fn task_mut(&mut self, id: TaskId) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    /// 直近の割り当て拒否理由。
    pub fn last_refusal(&self) -> Option<AssignRefusal> {
        self.last_refusal
    }

    /// 引き継ぎ材料を足す(担当が変わっても失われない)。
    pub fn add_context(&mut self, task_id: TaskId, item: impl Into<String>, now: Instant) {
        let Some(t) = self.task_mut(task_id) else {
            return;
        };
        let mut s: String = item.into();
        if s.chars().count() > CONTEXT_ITEM_MAX {
            s = s.chars().take(CONTEXT_ITEM_MAX).collect::<String>() + "…";
        }
        // 同じメモの連続追加はしない。再割り当てが空振りし続けると同一の
        // 「引き継ぎ」メモが 5 秒ごとに積まれ、前任者の本物の経過が
        // CONTEXT_CAP から押し出されてしまう。
        if t.context.last() == Some(&s) {
            return;
        }
        if t.context.len() >= CONTEXT_CAP {
            t.context.remove(0);
        }
        t.context.push(s);
        let n = t.context.len();
        t.record(now, TaskEvent::ContextCarried(n));
    }

    /// 次の担当へ渡す引き継ぎ文。これを [`MsgKind::Handoff`] で送る。
    pub fn handoff_brief(&self, task_id: TaskId) -> Option<String> {
        let t = self.task(task_id)?;
        let mut s = format!(
            "タスク #{} 「{}」を引き継ぎます。{}",
            t.id, t.title, t.description
        );
        if t.attempts > 0 {
            s.push_str(&format!(" (これまでの試行 {} 回)", t.attempts));
        }
        if !t.context.is_empty() {
            s.push_str(" これまでの経過: ");
            s.push_str(&t.context.join(" / "));
        }
        Some(s)
    }

    // ── 割り当て ────────────────────────────────────────────────────

    /// タスクを候補の中から 1 つのセッションへ割り当てる。
    ///
    /// 断った理由は [`Coordinator::last_refusal`] とタスク履歴に残る。
    /// 理由まで受け取りたいときは [`Coordinator::try_assign`] を使う。
    pub fn assign(&mut self, task_id: TaskId, candidates: &[SessionInfo]) -> Option<SessionId> {
        self.try_assign(task_id, candidates, Instant::now()).ok()
    }

    /// 割り当ての本体。断った理由を型で返す。
    ///
    /// 方針:
    /// - 空いているセッションを、忙しいセッションより優先する
    /// - `required_caps` に多く合致するセッションを優先する
    /// - **このタスクで失敗したセッションへは二度と割り当てない**
    /// - `max_attempts` を使い切ったら `NeedsUser` にして、それ以上回さない
    /// - **前任者の停止が未確認なら引き渡さない**(同時編集による破壊を防ぐ)
    /// - 同点なら ID の小さい方(決定的)
    pub fn try_assign(
        &mut self,
        task_id: TaskId,
        candidates: &[SessionInfo],
        now: Instant,
    ) -> Result<SessionId, AssignRefusal> {
        let max_attempts = self.limits.max_attempts;

        // ── 事前条件の確認 ──
        let (refusal, needs_user) = {
            let Some(t) = self.tasks.iter().find(|t| t.id == task_id) else {
                self.last_refusal = Some(AssignRefusal::NoSuchTask);
                return Err(AssignRefusal::NoSuchTask);
            };
            if t.state.is_terminal() {
                (Some(AssignRefusal::TaskFinished), false)
            } else if t.attempts >= max_attempts {
                (
                    Some(AssignRefusal::AttemptsExhausted {
                        attempts: t.attempts,
                    }),
                    true,
                )
            } else if let Some(prev) = t.assigned {
                if !t.prev_holder_stopped {
                    (
                        Some(AssignRefusal::PreviousHolderNotStopped { previous: prev }),
                        false,
                    )
                } else {
                    (None, false)
                }
            } else {
                (None, false)
            }
        };

        if let Some(r) = refusal {
            if needs_user {
                // 無限に回さない。人を呼んで終わりにする。
                let title = self
                    .task(task_id)
                    .map(|t| t.title.clone())
                    .unwrap_or_default();
                if let Some(t) = self.task_mut(task_id) {
                    t.state = TaskState::NeedsUser;
                    t.record(now, TaskEvent::HandoverRefused(r));
                    t.record(
                        now,
                        TaskEvent::EscalatedToUser("再試行の上限に達したため人手が必要です".into()),
                    );
                }
                self.escalate(
                    format!(
                        "タスク #{task_id}「{title}」は再試行の上限に達しました。担当を人が決めてください。"
                    ),
                    now,
                );
            } else if let Some(t) = self.task_mut(task_id) {
                t.record(now, TaskEvent::HandoverRefused(r));
            }
            self.last_refusal = Some(r);
            return Err(r);
        }

        // ── 候補の選定(純粋な方針・決定的) ──
        let (required, failed, previous) = {
            let t = self
                .tasks
                .iter()
                .find(|t| t.id == task_id)
                .expect("事前条件で存在は確認済み");
            (t.required_caps.clone(), t.failed_by.clone(), t.assigned)
        };

        let eligible: Vec<&SessionInfo> = candidates
            .iter()
            .filter(|c| assignable(c.state))
            .filter(|c| !failed.contains(&c.id))
            .collect();

        // ── ファイルの重なりは、配る瞬間に止める (fail-closed) ──
        //
        // 重なりは「渡す先」によって変わる(既に持っている本人へ渡すのは安全)
        // ので、候補ごとに判定して**重なりを持ち込む相手を選択肢から外す**。
        // 後勝ちにしない ＝ 断る方へ倒す。衝突をマージまで持ち越さないため。
        let admitted: Vec<&SessionInfo> = eligible
            .iter()
            .copied()
            .filter(|c| {
                self.tasks
                    .iter()
                    .find(|t| t.id == task_id)
                    .is_none_or(|t| admit(&self.tasks, t, c.id).is_ok())
            })
            .collect();

        if admitted.is_empty() && !eligible.is_empty() {
            // 生きている候補は居るのに、全員がファイルの重なりに当たる。
            // 「誰と・どのパターンで・どう分ければよいか」まで残して断る。
            let to = eligible[0].id;
            let verdict = self
                .tasks
                .iter()
                .find(|t| t.id == task_id)
                .map(|t| (admit(&self.tasks, t, to), t))
                .map(|(a, t)| (overlap_reason(&self.tasks, t, to, &a), a));
            if let Some((
                reason,
                Admit::Overlap {
                    with,
                    holder,
                    pattern,
                },
            )) = verdict
            {
                let refusal = AssignRefusal::FileOverlap { with, holder };
                let reason = reason.unwrap_or_else(|| refusal.label());
                if let Some(t) = self.task_mut(task_id) {
                    t.record(
                        now,
                        TaskEvent::OverlapRefused {
                            with,
                            holder,
                            pattern,
                            reason,
                        },
                    );
                }
                self.last_refusal = Some(refusal);
                return Err(refusal);
            }
        }

        let pick = admitted
            .iter()
            .min_by_key(|c| {
                let matched = required.iter().filter(|r| c.caps.contains(r)).count();
                // 能力の合致が多い順 → 空いている順 → ID の小さい順
                (std::cmp::Reverse(matched), busy_rank(c.state), c.id)
            })
            .map(|c| c.id);

        let Some(chosen) = pick else {
            self.last_refusal = Some(AssignRefusal::NoEligibleCandidate);
            if let Some(t) = self.task_mut(task_id) {
                t.record(
                    now,
                    TaskEvent::HandoverRefused(AssignRefusal::NoEligibleCandidate),
                );
            }
            return Err(AssignRefusal::NoEligibleCandidate);
        };

        // ── 確定 ──
        let reason = previous.map(|_| ReassignReason::Manual);
        if let Some(t) = self.task_mut(task_id) {
            t.assigned = Some(chosen);
            t.state = TaskState::Assigned;
            t.attempts = t.attempts.saturating_add(1);
            // 新しい担当が動き出す = また「停止未確認」に戻る。
            t.prev_holder_stopped = false;
            if previous.is_some() {
                t.record(
                    now,
                    TaskEvent::Reassigned {
                        from: previous,
                        to: chosen,
                        reason: reason.unwrap_or(ReassignReason::Manual),
                    },
                );
            }
            t.record(now, TaskEvent::Assigned(chosen));
        }
        self.last_refusal = None;
        Ok(chosen)
    }

    /// 引き継ぎ文をメッセージとして新担当の受信箱へ積む。
    ///
    /// 割り当ての直後に呼ぶ想定。前任者の作業内容を持ち越すことで、
    /// 新担当がゼロからやり直さずに済む。
    pub fn queue_handoff(&mut self, task_id: TaskId, to: SessionId, now: Instant) -> SendOutcome {
        let body = self
            .handoff_brief(task_id)
            .unwrap_or_else(|| format!("タスク #{task_id} を引き継ぎます。"));
        self.enqueue(
            AgentMessage::new(
                Endpoint::Supervisor,
                Endpoint::Session(to),
                MsgKind::Handoff,
                body,
            )
            .at(now),
        )
    }

    // ── 状態通知(監督レイヤから呼ばれる) ────────────────────────────

    /// タスクが動き出した。
    pub fn note_running(&mut self, task_id: TaskId, now: Instant) {
        if let Some(t) = self.task_mut(task_id) {
            if let Some(s) = t.assigned {
                t.state = TaskState::Running;
                t.record(now, TaskEvent::Started(s));
            }
        }
    }

    /// タスクが完了した。
    pub fn note_done(&mut self, task_id: TaskId, now: Instant) {
        if let Some(t) = self.task_mut(task_id) {
            let s = t.assigned.unwrap_or(0);
            t.state = TaskState::Done;
            t.record(now, TaskEvent::Completed(s));
        }
    }

    /// セッションが停滞した。担当中のタスクを `Stalled` にする。
    ///
    /// **停止は確認していない**ので、この時点では引き渡せない。
    /// 引き渡すには [`Coordinator::propose_stop`] → 承認 → 実際に停止 →
    /// [`Coordinator::confirm_stopped`] の順を踏む必要がある。
    pub fn note_stalled(&mut self, session: SessionId, now: Instant) {
        for t in self.tasks.iter_mut() {
            if t.assigned == Some(session) && !t.state.is_terminal() {
                t.state = TaskState::Stalled;
                // 一度停滞した相手へは戻さない。
                t.failed_by.insert(session);
                t.record(now, TaskEvent::Stalled(session));
            }
        }
    }

    /// セッションが失敗を報告した。停止確認は別途必要。
    pub fn note_failed(
        &mut self,
        task_id: TaskId,
        session: SessionId,
        reason: impl Into<String>,
        now: Instant,
    ) {
        if let Some(t) = self.task_mut(task_id) {
            t.state = TaskState::Failed;
            t.failed_by.insert(session);
            t.record(
                now,
                TaskEvent::Failed {
                    session,
                    reason: reason.into(),
                },
            );
        }
    }

    /// セッションのプロセスが消えた(終了 / クラッシュ)。
    ///
    /// プロセスが無い = **停止は確認済み**なので、そのまま引き渡してよい。
    pub fn note_exited(&mut self, session: SessionId, now: Instant) {
        for t in self.tasks.iter_mut() {
            if t.assigned == Some(session) && !t.state.is_terminal() {
                t.state = TaskState::Failed;
                t.failed_by.insert(session);
                t.prev_holder_stopped = true;
                t.record(
                    now,
                    TaskEvent::Failed {
                        session,
                        reason: "セッションが終了した".into(),
                    },
                );
                t.record(now, TaskEvent::PreviousStopped(session));
            }
        }
        self.unregister_session(session);
    }

    /// 前任者を止める提案を作る。既に停止確認済み / 担当不在なら None。
    ///
    /// 返った提案は [`gate_for`] で承認モードのゲートを通してから実行する。
    pub fn propose_stop(&self, task_id: TaskId) -> Option<Proposal> {
        let t = self.task(task_id)?;
        let s = t.assigned?;
        if t.prev_holder_stopped {
            return None;
        }
        Some(Proposal::StopSession {
            session: s,
            task: task_id,
            reason: format!(
                "タスク #{} 「{}」を別のエージェントへ引き継ぐため、前任 session:{} を停止します(作業中の内容が失われる可能性があります)",
                t.id, t.title, s
            ),
        })
    }

    /// 前任者が確実に止まったことを記録する。これで初めて引き渡せる。
    ///
    /// 呼び出し側は、実際に PTY の子プロセスが終了したことを
    /// (`Session::running() == false` などで)確認してから呼ぶこと。
    pub fn confirm_stopped(&mut self, task_id: TaskId, now: Instant) -> bool {
        let Some(t) = self.task_mut(task_id) else {
            return false;
        };
        let Some(s) = t.assigned else {
            return false;
        };
        t.prev_holder_stopped = true;
        t.record(now, TaskEvent::PreviousStopped(s));
        true
    }

    /// 停滞 / 死亡を受けての再割り当て。理由が履歴に残る。
    ///
    /// 前提条件(前任者の停止確認・失敗済みの除外・試行上限)は
    /// [`Coordinator::try_assign`] が全て見る。
    pub fn redispatch(
        &mut self,
        task_id: TaskId,
        candidates: &[SessionInfo],
        reason: ReassignReason,
        now: Instant,
    ) -> Result<(SessionId, SendOutcome), AssignRefusal> {
        let previous = self.task(task_id).and_then(|t| t.assigned);
        let chosen = self.try_assign(task_id, candidates, now)?;
        if let Some(t) = self.task_mut(task_id) {
            // try_assign が積んだ汎用の記録を、具体的な理由で上書き補足する。
            t.record(
                now,
                TaskEvent::Reassigned {
                    from: previous,
                    to: chosen,
                    reason,
                },
            );
        }
        // 引き継ぎ材料を新担当へ渡す。Dropped (レート制限等) は呼び出し側が
        // 警告できるよう結果ごと返す — 黙って握り潰すと新担当は何も知らされない。
        let outcome = self.queue_handoff(task_id, chosen, now);
        Ok((chosen, outcome))
    }

    /// ユーザーへ上げる。レート制限を通さずに直接積む(人を呼ぶ経路は塞がない)。
    pub fn escalate(&mut self, body: impl Into<String>, now: Instant) -> u64 {
        let id = self.next_msg_id;
        self.next_msg_id += 1;
        let msg = AgentMessage {
            id,
            from: Endpoint::Supervisor,
            to: Endpoint::User,
            kind: MsgKind::Escalation,
            body: body.into(),
            at: now,
            hops: 0,
        };
        if let Some(old) = self.user_inbox.push(msg) {
            self.record_drop(&old, DropReason::MailboxOverflow);
        }
        id
    }
}

// ── クォータ監視 (プラン枠の横断集約 + 枯渇予測) ──────────────────────
//
// ここから下だけが例外的に別モジュール (`quota`) を使う。コアの
// `Coordinator` は従来どおり他モジュールへ依存しない。
//
// `quota` は `#[path]` でこのモジュールの子として取り込む。main.rs を
// 触らずに配線できるようにするためで、**main.rs へ `mod quota;` を足す
// 必要は無い** (足すなら下の 1 行を消し、`quota::` を `crate::quota::`
// へ書き換えること。二重登録は型が別物になる)。

/// プラン使用量の読み取り・集約・助言 (詳細はモジュール doc)。
#[path = "quota.rs"]
pub mod quota;

use std::sync::mpsc::{self, Receiver};
use std::time::{SystemTime, UNIX_EPOCH};

/// トークン集計 1 回ぶん。同じ走査から 3 つの窓を切り出す。
///
/// 窓ごとに読み直すと背景 I/O が 3 倍になるので、
/// [`quota::scan_tokens_multi_in`] が 1 パスで振り分ける (設計原則 3)。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenScan {
    /// 直近 [`quota::TOKEN_WINDOW`] (表示用の「直近 24 時間」)。
    pub window: Vec<quota::AgentTokens>,
    /// 今日 (UTC) ぶん。日次の上限判定に使う。
    pub today: Vec<quota::AgentTokens>,
    /// このアプリを起動してからぶん。セッションの上限判定に使う。
    pub session: Vec<quota::AgentTokens>,
}

/// 背景スレッド 1 回分の読み取り結果。
///
/// トークン集計は使用率よりずっと重い ([`quota::TOKEN_TTL`]) ので、
/// 読まなかった回は `None` = 前の結果を据え置く。
type ScanResult = (Vec<quota::QuotaSnapshot>, Option<TokenScan>);

/// ベンダーファイルを読み直す間隔。ファイルの更新はターン単位なので
/// 短くしすぎない (UI は毎フレーム [`QuotaWatch::refresh_if_stale`] を呼べる)。
pub const QUOTA_TTL: Duration = Duration::from_secs(20);

/// 保持する観測イベントの上限 (メモリを有界に保つ)。
pub const QUOTA_EVENT_CAP: usize = 256;

/// クォータの背景監視。
///
/// **UI スレッドではファイルを触らない**。git.rs / git_panel.rs と同じく
/// 「TTL 切れ → バックグラウンドスレッド起動 → チャネルで受け取る」形。
///
/// ## 描画契約 (パネル側が使う API)
///
/// 1. 毎フレーム [`QuotaWatch::refresh_if_stale`] を呼ぶ (安価。TTL 内は即戻る)。
/// 2. 走行本数を [`QuotaWatch::set_running`] で渡す (bin 名 → 本数)。
/// 3. 表示は [`QuotaWatch::accounts`] の [`quota::AccountUsage`] を 1 行 1 アカウントで。
///    - `used_fraction` が `None` の行に数字を出さない (「不明」と描く)
///    - `confidence` が [`quota::Confidence::Measured`] 以外なら「推定」と明示する
///    - `projection` は [`quota::Projection::InsufficientData`] のとき
///      「データ不足」と描き、決して 0 分などと描かない
/// 4. 助言は [`QuotaWatch::advice`] / [`QuotaWatch::worst_advice`]。
///    `severity()` 0/1/2 で色分けし、`message()` をそのまま出す。
///    **自動で止めない**。止めるかどうかは人が決める。
/// 5. セッションの出力から上限警告を拾ったら [`QuotaWatch::note_rate_limited`]
///    (agents.rs の `SessionEvent::RateLimited` をそのまま流せる)。
pub struct QuotaWatch {
    pending: Option<Receiver<ScanResult>>,
    snapshots: Vec<quota::QuotaSnapshot>,
    tokens: TokenScan,
    events: Vec<quota::RateLimitEvent>,
    history: quota::BurnHistory,
    running: Vec<(String, usize)>,
    policy: quota::Policy,
    last_refresh: Option<Instant>,
    /// トークン集計を最後に読んだ時刻 ([`quota::TOKEN_TTL`] で間引く)。
    last_token_scan: Option<Instant>,
    /// このアプリが起動した時刻 (セッション上限の起点)。
    session_since: SystemTime,
    /// 「今日ぶん」を数えた日 (UTC の通し番号)。ここが変われば日次はリセット。
    token_day: u64,
    /// 取り込みに成功した回数 (テスト・診断用)。
    applied: u64,
}

impl Default for QuotaWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl QuotaWatch {
    pub fn new() -> Self {
        Self {
            pending: None,
            snapshots: Vec::new(),
            tokens: TokenScan::default(),
            events: Vec::new(),
            history: quota::BurnHistory::new(),
            running: Vec::new(),
            policy: quota::Policy::default(),
            last_refresh: None,
            last_token_scan: None,
            session_since: SystemTime::now(),
            token_day: quota::utc_day_index(SystemTime::now()),
            applied: 0,
        }
    }

    /// セッション上限の起点を差し替える (時刻を注入したいテスト用)。
    pub fn set_session_since(&mut self, at: SystemTime) {
        self.session_since = at;
    }

    /// 日 (UTC) をまたいでいたら「今日ぶん」を捨てて読み直させる。跨いだら true。
    ///
    /// 集計は [`quota::TOKEN_TTL`] 間隔でしか回らないので、放っておくと
    /// 日をまたいだ直後に**前日の額**が数分残る。日次の上限は「その日ぶん」
    /// なので、境界を越えた時点で 0 に戻す (前日の額で送信を止め続けない)。
    /// 「このセッションぶん」は日をまたいでも続くので触らない。
    ///
    /// 日の切り方 (UTC) の理由は [`quota::utc_day_index`] を参照。
    pub fn roll_day_if_needed(&mut self, now: SystemTime) -> bool {
        let day = quota::utc_day_index(now);
        if self.token_day == day {
            return false;
        }
        self.token_day = day;
        self.tokens.today.clear();
        // 次の refresh で必ず読み直す (TTL の途中でも待たせない)。
        self.last_token_scan = None;
        true
    }

    /// しきい値を差し替える (設定から)。
    pub fn set_policy(&mut self, policy: quota::Policy) {
        self.policy = policy;
    }

    pub fn policy(&self) -> &quota::Policy {
        &self.policy
    }

    /// 走行本数 (bin 名 → 本数) を更新する。
    pub fn set_running(&mut self, running: Vec<(String, usize)>) {
        self.running = running;
    }

    /// この枠で走っている合計本数。
    pub fn running_total(&self) -> usize {
        self.running.iter().map(|(_, n)| *n).sum()
    }

    /// 上限警告を 1 件記録する (検知済みの行を渡す)。
    pub fn note_rate_limited(&mut self, agent: &str, line: &str, at: SystemTime) {
        self.push_event(quota::RateLimitEvent {
            agent: agent.to_string(),
            at,
            line: line.to_string(),
        });
    }

    /// 端末出力から上限警告を拾って記録する (検知は terminal.rs の再利用)。
    /// 拾えたら true。
    pub fn note_output(&mut self, agent: &str, text: &str, at: SystemTime) -> bool {
        match quota::observe_output(agent, text, at) {
            Some(e) => {
                self.push_event(e);
                true
            }
            None => false,
        }
    }

    fn push_event(&mut self, e: quota::RateLimitEvent) {
        self.events.push(e);
        if self.events.len() > QUOTA_EVENT_CAP {
            let cut = self.events.len() - QUOTA_EVENT_CAP;
            self.events.drain(..cut);
        }
        quota::merge_observed(&mut self.snapshots, &self.events);
    }

    /// TTL 切れならバックグラウンドで読み直す (毎フレーム呼んで安全)。
    pub fn refresh_if_stale(&mut self) {
        self.refresh(false);
    }

    /// TTL を無視して読み直す (ウィンドウのフォーカス復帰など)。
    pub fn force_refresh(&mut self) {
        self.refresh(true);
    }

    fn refresh(&mut self, force: bool) {
        // 1) 完了した読み取りがあれば取り込む
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok((snaps, tokens)) => {
                    if let Some(t) = tokens {
                        self.tokens = t;
                    }
                    self.apply(snaps, SystemTime::now());
                    self.pending = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.pending.is_some() {
            return;
        }
        if !force {
            if let Some(t) = self.last_refresh {
                if t.elapsed() < QUOTA_TTL {
                    return;
                }
            }
        }
        // 失敗時も時刻は更新し、毎フレーム再起動しない。
        self.last_refresh = Some(Instant::now());
        // 日 (UTC) をまたいだら「今日ぶん」を捨てる。ここで見るので
        // 判定の遅れは最大でも QUOTA_TTL で済む (毎フレームは見ない)。
        self.roll_day_if_needed(SystemTime::now());
        // トークン集計は何十本ものトランスクリプトを舐めるので、使用率と
        // 同じ間隔では回さない (アイドル時の背景 I/O を抑える)。
        let scan_tokens = self
            .last_token_scan
            .map(|t| t.elapsed() >= quota::TOKEN_TTL)
            .unwrap_or(true);
        if scan_tokens {
            self.last_token_scan = Some(Instant::now());
        }
        let session_since = self.session_since;
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("zv-quota".into())
            .spawn(move || {
                // 使用率とトークン消費は同じ 1 本のスレッドで読む
                // (エージェント本数ぶんスレッドを増やさない)。
                let tokens = scan_tokens.then(|| {
                    let now = SystemTime::now();
                    let window = now.checked_sub(quota::TOKEN_WINDOW).unwrap_or(UNIX_EPOCH);
                    // セッションの起点は窓より古くしない。トランスクリプトは
                    // 窓のぶんしか読まないので、それより前へ遡っても数字は
                    // 増えず、走査するファイルだけが際限なく増えてしまう。
                    let session = session_since.max(window);
                    let sinces = [window, quota::utc_day_start(now), session];
                    let mut got = match dirs::home_dir() {
                        Some(h) => quota::scan_tokens_multi_in(&h, &sinces),
                        None => Vec::new(),
                    };
                    let mut take = |i: usize| -> Vec<quota::AgentTokens> {
                        got.get_mut(i).map(std::mem::take).unwrap_or_default()
                    };
                    TokenScan {
                        window: take(0),
                        today: take(1),
                        session: take(2),
                    }
                });
                let _ = tx.send((quota::snapshot_all(), tokens));
            });
        if spawned.is_ok() {
            self.pending = Some(rx);
        }
    }

    /// 読み取り結果の取り込み (時刻を注入できるのでテスト可能)。
    pub fn apply(&mut self, mut snaps: Vec<quota::QuotaSnapshot>, now: SystemTime) {
        quota::merge_observed(&mut snaps, &self.events);
        // 使用率の観測点を履歴へ積む (燃焼速度の材料)。測定時刻が分かれば
        // それを使い、無ければ取り込み時刻で代用する。
        for u in quota::aggregate(&snaps, &self.running) {
            if let Some(f) = u.used_fraction {
                let at = snaps
                    .iter()
                    .filter(|s| s.account == u.account)
                    .filter_map(|s| s.measured_at)
                    .max()
                    .unwrap_or(now);
                self.history.record(&u.account, at, f);
            }
        }
        self.snapshots = snaps;
        self.applied += 1;
    }

    /// 最新のスナップショット (エージェント単位)。
    pub fn snapshots(&self) -> &[quota::QuotaSnapshot] {
        &self.snapshots
    }

    /// 取り込み回数。
    pub fn applied(&self) -> u64 {
        self.applied
    }

    /// 観測した上限イベント (時刻昇順)。
    pub fn events(&self) -> &[quota::RateLimitEvent] {
        &self.events
    }

    /// アカウント単位の集約 + 枯渇予測。
    pub fn accounts(&self, now: SystemTime) -> Vec<quota::AccountUsage> {
        let mut out = quota::aggregate(&self.snapshots, &self.running);
        for u in out.iter_mut() {
            let burn = self.history.rate(&u.account, now);
            quota::attach_projection(u, burn.as_ref(), now);
        }
        out
    }

    /// アカウントごとの助言。
    pub fn advice(&self, now: SystemTime) -> Vec<(String, quota::Advice)> {
        self.accounts(now)
            .into_iter()
            .map(|u| {
                let a = quota::advise(&u, u.running_agents, &self.policy, now);
                (u.account, a)
            })
            .collect()
    }

    /// 最も深刻な助言 (無ければ [`quota::Advice::Ok`])。ステータスバー向け。
    pub fn worst_advice(&self, now: SystemTime) -> quota::Advice {
        self.advice(now)
            .into_iter()
            .map(|(_, a)| a)
            .max_by_key(|a| a.severity())
            .unwrap_or(quota::Advice::Ok)
    }

    /// 直近 [`quota::TOKEN_WINDOW`] のトークン消費 (消費の多い順)。
    /// **消費ゼロのエージェントは入っていない** ので、空なら 1px も出さない。
    pub fn tokens(&self) -> &[quota::AgentTokens] {
        &self.tokens.window
    }

    /// 3 つの窓ぶんの集計そのもの。
    pub fn token_scan(&self) -> &TokenScan {
        &self.tokens
    }

    /// 読み取り結果を直接差し込む (時刻・I/O を注入したいテスト用)。
    pub fn set_tokens(&mut self, tokens: Vec<quota::AgentTokens>) {
        self.tokens.window = tokens;
    }

    /// 3 つの窓ぶんをまとめて差し込む (時刻・I/O を注入したいテスト用)。
    pub fn set_token_scan(&mut self, scan: TokenScan) {
        self.tokens = scan;
    }

    /// 全エージェントを合算したトークン消費。1 件も無ければ None。
    pub fn tokens_total(&self) -> Option<quota::TokenUsage> {
        if self.tokens.window.is_empty() {
            return None;
        }
        let mut t = quota::TokenUsage::default();
        for a in &self.tokens.window {
            t.add(&a.total);
        }
        Some(t)
    }

    /// 全エージェントを合算した推定コスト。1 件も無ければ None。
    pub fn cost_total(&self, prices: &dyn quota::PriceLookup) -> Option<quota::CostEstimate> {
        if self.tokens.window.is_empty() {
            return None;
        }
        Some(Self::sum_cost(&self.tokens.window, prices))
    }

    /// 今日 (UTC) ぶんの推定コスト額。消費が無ければ 0.0。
    pub fn cost_today(&self, prices: &dyn quota::PriceLookup) -> f64 {
        Self::sum_cost(&self.tokens.today, prices).amount
    }

    /// このアプリを起動してからの推定コスト額。消費が無ければ 0.0。
    pub fn cost_session(&self, prices: &dyn quota::PriceLookup) -> f64 {
        Self::sum_cost(&self.tokens.session, prices).amount
    }

    /// エージェント別の集計を 1 つの推定コストへ畳む。
    fn sum_cost(
        list: &[quota::AgentTokens],
        prices: &dyn quota::PriceLookup,
    ) -> quota::CostEstimate {
        let mut est = quota::CostEstimate::default();
        for a in list {
            let e = quota::estimate_cost(a, prices);
            est.amount += e.amount;
            est.unknown_tokens = est.unknown_tokens.saturating_add(e.unknown_tokens);
            for m in e.unknown_models {
                if !est.unknown_models.contains(&m) {
                    est.unknown_models.push(m);
                }
            }
        }
        est.unknown_models.sort();
        est
    }
}

// ── テスト ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn msg(from: SessionId, to: SessionId, at: Instant) -> AgentMessage {
        AgentMessage::new(
            Endpoint::Session(from),
            Endpoint::Session(to),
            MsgKind::Request,
            "テスト本文",
        )
        .at(at)
    }

    // ── 配達の安全性 ────────────────────────────────────────────────

    /// 安全な状態の集合が明示的であること。
    #[test]
    fn deliverable_safe_set_is_explicit() {
        assert!(deliverable(SessionState::Idle));
        assert!(deliverable(SessionState::AwaitingInput));
        assert!(!deliverable(SessionState::Working));
        assert!(!deliverable(SessionState::WaitingApproval));
        assert!(!deliverable(SessionState::Stalled));
        assert!(!deliverable(SessionState::Exited));
        // 状態が分からないときは既定で配達しない。
        assert!(!deliverable(SessionState::Unknown));
    }

    /// 生成中のセッションへは配達しない(PTY へ書くと入力が壊れる)。
    #[test]
    fn no_delivery_while_working() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        let now = t0();
        assert!(matches!(
            c.enqueue(msg(1, 2, now)),
            SendOutcome::Queued { .. }
        ));

        let out = c.take_deliverable(&[(2, SessionState::Working)]);
        assert!(out.is_empty(), "作業中に配達してはいけない");
        // メッセージは消えずに残っている。
        assert_eq!(c.mailbox(2).unwrap().len(), 1);
    }

    /// 承認待ちへは絶対に配達しない。
    /// 本文がそのまま承認の返事として解釈されてしまうため。
    #[test]
    fn no_delivery_while_waiting_approval() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        let now = t0();
        c.enqueue(msg(1, 2, now));

        let out = c.take_deliverable(&[(2, SessionState::WaitingApproval)]);
        assert!(out.is_empty(), "承認待ちに配達してはいけない");
        assert_eq!(c.mailbox(2).unwrap().len(), 1);
        assert_eq!(c.mailbox(2).unwrap().delivered(), 0);
    }

    /// 状態が一覧に無い(= 不明な)セッションへは配達しない。
    #[test]
    fn no_delivery_when_state_unknown() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        c.enqueue(msg(1, 2, t0()));

        assert!(c.take_deliverable(&[]).is_empty());
        assert!(c.take_deliverable(&[(2, SessionState::Unknown)]).is_empty());
        assert_eq!(c.mailbox(2).unwrap().len(), 1);
    }

    /// 待機中なら予約され、ACK 後にだけ受信箱から消える。
    #[test]
    fn delivers_when_idle() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        c.enqueue(msg(1, 2, t0()));

        let out = c.take_deliverable(&[(2, SessionState::Idle)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session, 2);
        assert!(
            out[0].text.starts_with(INJECT_PREFIX),
            "機械注入の目印が要る"
        );
        assert!(!out[0].text.ends_with('\r'), "確定キーは submit が別送する");
        assert!(out[0].text.contains("session:1"));
        assert_eq!(c.mailbox(2).unwrap().len(), 1, "ACK 前には消さない");
        assert_eq!(c.mailbox(2).unwrap().delivered(), 0);
        assert!(
            c.take_deliverable(&[(2, SessionState::Idle)]).is_empty(),
            "同じ予約を二重送信しない"
        );
        assert!(c.finish_delivery(2, out[0].msg_id, true, t0()));
        assert_eq!(c.mailbox(2).unwrap().len(), 0);
        assert_eq!(c.mailbox(2).unwrap().delivered(), 1);
    }

    /// 予約 ID と違う ACK では受信箱を減らさない。
    #[test]
    fn mismatched_ack_never_pops_reserved_message() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        c.enqueue(msg(1, 2, t0()));

        let out = c.take_deliverable(&[(2, SessionState::Idle)]);
        let msg_id = out[0].msg_id;
        assert!(!c.finish_delivery(2, msg_id + 1, true, t0()));
        assert_eq!(c.mailbox(2).unwrap().len(), 1);
        assert_eq!(c.mailbox(2).unwrap().delivered(), 0);
        assert!(
            c.take_deliverable(&[(2, SessionState::Idle)]).is_empty(),
            "正しい ACK まで同じ本文を二重予約しない"
        );
        assert!(c.finish_delivery(2, msg_id, true, t0()));
    }

    /// 1 フレームで 1 セッションにつき 1 通だけ配る(連打で入力を壊さない)。
    #[test]
    fn delivers_one_per_session_per_call() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        let now = t0();
        c.enqueue(msg(1, 2, now));
        c.enqueue(msg(1, 2, now));

        let out = c.take_deliverable(&[(2, SessionState::Idle)]);
        assert_eq!(out.len(), 1);
        assert_eq!(c.mailbox(2).unwrap().len(), 2, "予約だけでは減らない");
        assert!(c.finish_delivery(2, out[0].msg_id, true, t0()));
        assert_eq!(c.mailbox(2).unwrap().len(), 1);
        assert_eq!(
            c.take_deliverable(&[(2, SessionState::Idle)]).len(),
            1,
            "ACK 後に次の 1 通を予約できる"
        );
    }

    /// submit が失敗した場合は無限再投入せず、人へ理由を返す。
    #[test]
    fn failed_delivery_is_escalated_once() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        c.enqueue(msg(1, 2, t0()));

        let out = c.take_deliverable(&[(2, SessionState::Idle)]);
        assert_eq!(out.len(), 1);
        assert!(!c.finish_delivery(2, out[0].msg_id, false, t0()));
        assert!(c.mailbox(2).unwrap().is_empty());
        assert_eq!(
            c.mailbox(2).unwrap().delivered(),
            0,
            "失敗は配達数に入れない"
        );
        let notices = c.take_user_messages();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].body.contains("配達できませんでした"));
        assert!(c.take_deliverable(&[(2, SessionState::Idle)]).is_empty());
    }

    /// submit キュー拒否は連打せず、有界再試行の後に人へ返す。
    #[test]
    fn queue_rejection_backs_off_then_escalates_once() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        let mut now = t0();
        c.enqueue(msg(1, 2, now));

        for attempt in 1..=DELIVERY_QUEUE_RETRY_MAX {
            let out = c.take_deliverable_at(&[(2, SessionState::Idle)], now);
            assert_eq!(out.len(), 1, "{attempt} 回目を予約できない");
            let abandoned = c.defer_delivery(2, out[0].msg_id, now);
            assert_eq!(abandoned, attempt == DELIVERY_QUEUE_RETRY_MAX);

            if attempt < DELIVERY_QUEUE_RETRY_MAX {
                assert_eq!(c.mailbox(2).unwrap().len(), 1, "再試行前は本文を保持");
                assert!(
                    c.take_deliverable_at(
                        &[(2, SessionState::Idle)],
                        now + DELIVERY_QUEUE_RETRY_BACKOFF - Duration::from_millis(1),
                    )
                    .is_empty(),
                    "バックオフ中は予約しない"
                );
                now += DELIVERY_QUEUE_RETRY_BACKOFF;
            }
        }

        assert!(c.mailbox(2).unwrap().is_empty());
        assert_eq!(c.mailbox(2).unwrap().delivered(), 0);
        let notices = c.take_user_messages();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].body.contains("submit キュー"));
        assert!(c.take_user_messages().is_empty(), "最終失敗は 1 回だけ表示");
    }

    /// 受信箱が溢れても ACK 待ちの先頭は押し出さない。
    #[test]
    fn mailbox_overflow_preserves_in_flight_front() {
        let limits = Limits {
            mailbox_cap: 1,
            ..Limits::default()
        };
        let mut c = Coordinator::with_limits(limits);
        c.register_session(1);
        c.register_session(2);
        c.register_session(3);
        c.enqueue(msg(1, 2, t0()));

        let reserved = c.take_deliverable(&[(2, SessionState::Idle)])[0].msg_id;
        c.enqueue(msg(3, 2, t0()));
        assert_eq!(
            c.mailbox(2).unwrap().len(),
            2,
            "cap=1 でも予約中は 1 件だけ保護"
        );
        assert_eq!(c.mailbox(2).unwrap().iter().next().unwrap().id, reserved);
        assert!(c.finish_delivery(2, reserved, true, t0()));
        assert_eq!(c.mailbox(2).unwrap().len(), 1);
        assert_eq!(c.mailbox(2).unwrap().delivered(), 1);
    }

    /// 注入本文の改行と制御文字は潰す(途中で送信されてしまうのを防ぐ)。
    #[test]
    fn injection_body_is_single_line() {
        let m = AgentMessage::new(
            Endpoint::Supervisor,
            Endpoint::Session(1),
            MsgKind::Status,
            "一行目\n二行目\r\n三行目\x07",
        );
        let text = format_injection(&m);
        assert_eq!(text.matches('\r').count(), 1, "末尾の CR 以外に CR は無い");
        assert!(!text.contains('\n'));
        assert!(text.contains("一行目 / 二行目 / 三行目"));
    }

    // ── ループとストーム抑制 ─────────────────────────────────────────

    /// ホップ上限を超えた転送は捨て、理由を記録する。
    #[test]
    fn hop_limit_drops_forwarded_message_with_reason() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        c.register_session(3);
        let now = t0();

        let mut m = msg(1, 2, now);
        m.hops = c.limits().max_hops; // 次の転送で上限超過
        let out = c.forward(m, Endpoint::Session(3), now);

        match out {
            SendOutcome::Dropped {
                reason: DropReason::HopLimit { hops },
            } => assert_eq!(hops, DEFAULT_MAX_HOPS + 1),
            other => panic!("ホップ上限で捨てるはず: {other:?}"),
        }
        assert_eq!(c.drop_count(DropKind::HopLimit), 1);
        let rec = c.drop_log().last().expect("破棄ログが残る");
        assert_eq!(rec.reason.kind(), DropKind::HopLimit);
        assert!(!rec.reason.label().is_empty());
        // 宛先には届いていない。
        assert_eq!(c.mailbox(3).unwrap().len(), 0);
    }

    /// 上限内の転送は通る(上限そのものが誤爆していないことの確認)。
    #[test]
    fn forward_below_hop_limit_passes() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        c.register_session(3);
        let now = t0();
        let m = msg(1, 2, now); // hops = 0 → 転送で 1
        assert!(matches!(
            c.forward(m, Endpoint::Session(3), now),
            SendOutcome::Queued { .. }
        ));
        assert_eq!(c.drop_count(DropKind::HopLimit), 0);
    }

    /// ピンポンを検出したら抑制し、ユーザーへエスカレーションする。
    #[test]
    fn pingpong_suppressed_and_escalated_to_user() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        let base = t0();

        // 交互に往復させる。しきい値は「向き無視の合計本数」。
        let limit = c.limits().pingpong_limit;
        for i in 0..limit {
            let at = base + Duration::from_millis(100 * i as u64);
            let out = if i % 2 == 0 {
                c.enqueue(msg(1, 2, at))
            } else {
                c.enqueue(msg(2, 1, at))
            };
            assert!(
                matches!(out, SendOutcome::Queued { .. }),
                "{i} 本目までは通るはず"
            );
        }
        assert_eq!(c.drop_count(DropKind::PingPong), 0);

        // しきい値到達後の 1 本は抑制される。
        let at = base + Duration::from_millis(100 * limit as u64);
        let out = c.enqueue(msg(1, 2, at));
        assert_eq!(
            out,
            SendOutcome::Dropped {
                reason: DropReason::PingPong
            }
        );
        assert_eq!(c.drop_count(DropKind::PingPong), 1);

        // ユーザーへ 1 通だけエスカレーションされている。
        let esc = c.take_user_messages();
        assert_eq!(esc.len(), 1, "人へ上げるのはペアにつき 1 回");
        assert_eq!(esc[0].to, Endpoint::User);
        assert_eq!(esc[0].kind, MsgKind::Escalation);
        assert!(esc[0].body.contains("session:1"));
        assert!(esc[0].body.contains("session:2"));
    }

    /// 同一ペアのレート制限。抑制されるだけでなく、件数が数えられている。
    #[test]
    fn pair_rate_limit_suppresses_and_counts() {
        // ピンポン判定と混ざらないよう、片方向だけへ送る構成にする。
        let limits = Limits {
            pingpong_limit: 1_000,
            ..Limits::default()
        };
        let mut c = Coordinator::with_limits(limits);
        c.register_session(1);
        c.register_session(2);
        let base = t0();
        let pair_limit = c.limits().pair_limit;

        for i in 0..pair_limit {
            let at = base + Duration::from_millis(10 * i as u64);
            assert!(matches!(
                c.enqueue(msg(1, 2, at)),
                SendOutcome::Queued { .. }
            ));
        }
        // 超過分は 3 本まとめて捨てられ、3 と数えられる。
        for i in 0..3u32 {
            let at = base + Duration::from_millis(1000 + i as u64);
            assert_eq!(
                c.enqueue(msg(1, 2, at)),
                SendOutcome::Dropped {
                    reason: DropReason::RateLimitPair
                }
            );
        }
        assert_eq!(c.drop_count(DropKind::RateLimitPair), 3, "捨てた数を数える");
        assert_eq!(c.total_drops(), 3);
        assert_eq!(c.drop_log().count(), 3);

        // 窓が過ぎればまた通る。
        let later = base + DEFAULT_WINDOW + Duration::from_secs(1);
        assert!(matches!(
            c.enqueue(msg(1, 2, later)),
            SendOutcome::Queued { .. }
        ));
    }

    /// 一斉送信は直接送信よりきつく絞る。
    #[test]
    fn broadcast_is_limited_harder_than_direct() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);
        c.register_session(3);
        let base = t0();
        assert!(c.limits().broadcast_limit < c.limits().pair_limit);

        for i in 0..c.limits().broadcast_limit {
            let at = base + Duration::from_millis(10 * i as u64);
            let out = c.enqueue(
                AgentMessage::new(
                    Endpoint::Session(1),
                    Endpoint::Broadcast,
                    MsgKind::Status,
                    "全員へ",
                )
                .at(at),
            );
            // 送信元 1 を除く 2 つの受信箱へ入る。
            assert_eq!(
                out,
                SendOutcome::Broadcast {
                    id: i as u64 + 1,
                    delivered_to: 2
                }
            );
        }
        let at = base + Duration::from_millis(500);
        assert_eq!(
            c.enqueue(
                AgentMessage::new(
                    Endpoint::Session(1),
                    Endpoint::Broadcast,
                    MsgKind::Status,
                    "全員へ",
                )
                .at(at)
            ),
            SendOutcome::Dropped {
                reason: DropReason::RateLimitBroadcast
            }
        );
        assert_eq!(c.drop_count(DropKind::RateLimitBroadcast), 1);
    }

    /// 全体のレート制限も数えられる。
    #[test]
    fn global_rate_limit_counts() {
        let limits = Limits {
            global_limit: 3,
            pair_limit: 1_000,
            pingpong_limit: 1_000,
            ..Limits::default()
        };
        let mut c = Coordinator::with_limits(limits);
        c.register_session(1);
        c.register_session(2);
        let base = t0();
        for i in 0..3 {
            let at = base + Duration::from_millis(i);
            assert!(matches!(
                c.enqueue(msg(1, 2, at)),
                SendOutcome::Queued { .. }
            ));
        }
        assert_eq!(
            c.enqueue(msg(1, 2, base + Duration::from_millis(10))),
            SendOutcome::Dropped {
                reason: DropReason::RateLimitGlobal
            }
        );
        assert_eq!(c.drop_count(DropKind::RateLimitGlobal), 1);
    }

    /// 存在しないセッション宛は理由付きで捨てる(黙って消さない)。
    #[test]
    fn unknown_target_is_recorded() {
        let mut c = Coordinator::new();
        c.register_session(1);
        assert_eq!(
            c.enqueue(msg(1, 99, t0())),
            SendOutcome::Dropped {
                reason: DropReason::UnknownTarget
            }
        );
        assert_eq!(c.drop_count(DropKind::UnknownTarget), 1);
    }

    // ── メールボックス ──────────────────────────────────────────────

    /// 受信箱は上限を超えたら古いものから捨て、その件数を報告する。
    #[test]
    fn mailbox_ring_drops_oldest_and_counts() {
        let limits = Limits {
            mailbox_cap: 4,
            pair_limit: 1_000,
            global_limit: 1_000,
            pingpong_limit: 1_000,
            ..Limits::default()
        };
        let mut c = Coordinator::with_limits(limits);
        c.register_session(1);
        c.register_session(2);
        let base = t0();

        let mut ids = Vec::new();
        for i in 0..7u64 {
            let at = base + Duration::from_millis(i);
            match c.enqueue(msg(1, 2, at)) {
                SendOutcome::Queued { id } => ids.push(id),
                other => panic!("積まれるはず: {other:?}"),
            }
        }

        let mb = c.mailbox(2).unwrap();
        assert_eq!(mb.len(), 4, "上限を超えて伸びない");
        assert_eq!(mb.dropped_oldest(), 3, "溢れて捨てた件数を数える");
        // 残っているのは新しい 4 通(古い 3 通が押し出された)。
        let remaining: Vec<u64> = mb.iter().map(|m| m.id).collect();
        assert_eq!(remaining, ids[3..].to_vec());
        // 押し出しも破棄ログに理由付きで残る。
        assert_eq!(c.drop_count(DropKind::MailboxOverflow), 3);
    }

    // ── 割り当て ────────────────────────────────────────────────────

    fn cands() -> Vec<SessionInfo> {
        vec![
            SessionInfo::new(1, SessionState::Idle, &["rust"]),
            SessionInfo::new(2, SessionState::Working, &["rust", "test"]),
            SessionInfo::new(3, SessionState::Idle, &["docs"]),
        ]
    }

    /// 能力が合致し、かつ空いているセッションを選ぶ。
    #[test]
    fn assign_prefers_capable_then_idle() {
        let mut c = Coordinator::new();
        let now = t0();
        let t = c.add_task("実装", "rust を書く", &["rust"], now);
        // 1 と 2 が rust 持ち。合致数は同じ 1 なので、空いている 1 が勝つ。
        assert_eq!(c.try_assign(t, &cands(), now), Ok(1));
    }

    /// 能力の合致数が多い方を、忙しくても優先する。
    #[test]
    fn assign_prefers_more_capability_matches() {
        let mut c = Coordinator::new();
        let now = t0();
        let t = c.add_task("実装", "rust とテスト", &["rust", "test"], now);
        // 2 は 2 つ合致(作業中)、1 は 1 つ合致(空き)→ 合致数が優先。
        assert_eq!(c.try_assign(t, &cands(), now), Ok(2));
    }

    /// 同じ入力なら必ず同じ結果になる(決定的)。
    #[test]
    fn assign_is_deterministic() {
        let now = t0();
        // 完全に横並びの候補。並び順を変えても結果が変わらないこと。
        let a = vec![
            SessionInfo::new(7, SessionState::Idle, &["rust"]),
            SessionInfo::new(3, SessionState::Idle, &["rust"]),
            SessionInfo::new(5, SessionState::Idle, &["rust"]),
        ];
        let mut b = a.clone();
        b.reverse();

        for _ in 0..10 {
            let mut c1 = Coordinator::new();
            let t1 = c1.add_task("T", "d", &["rust"], now);
            let mut c2 = Coordinator::new();
            let t2 = c2.add_task("T", "d", &["rust"], now);
            // 同点なら ID の小さい方。
            assert_eq!(c1.try_assign(t1, &a, now), Ok(3));
            assert_eq!(c2.try_assign(t2, &b, now), Ok(3));
        }
    }

    /// 終了済み / 停滞中のセッションへは割り当てない。
    #[test]
    fn assign_skips_unusable_sessions() {
        let mut c = Coordinator::new();
        let now = t0();
        let t = c.add_task("T", "d", &[], now);
        let list = vec![
            SessionInfo::new(1, SessionState::Exited, &[]),
            SessionInfo::new(2, SessionState::Stalled, &[]),
            SessionInfo::new(3, SessionState::Unknown, &[]),
            SessionInfo::new(4, SessionState::Idle, &[]),
        ];
        assert_eq!(c.try_assign(t, &list, now), Ok(4));
    }

    /// そのタスクで一度失敗したセッションへは、二度と割り当てない。
    #[test]
    fn assign_never_returns_previously_failed_session() {
        let mut c = Coordinator::new();
        let now = t0();
        let t = c.add_task("T", "d", &["rust"], now);

        // 1 に割り当てて失敗させる。
        assert_eq!(c.try_assign(t, &cands(), now), Ok(1));
        c.note_failed(t, 1, "ビルドが通らない", now);
        c.confirm_stopped(t, now);
        assert!(c.task(t).unwrap().has_failed(1));

        // 1 は候補に残っているが選ばれない。
        let next = c.try_assign(t, &cands(), now).expect("2 が選ばれる");
        assert_ne!(next, 1, "失敗したセッションへ戻してはいけない");
        assert_eq!(next, 2);

        // 2 も失敗 → 残るのは 3 のみ。
        c.note_failed(t, 2, "タイムアウト", now);
        c.confirm_stopped(t, now);
        let third = c.try_assign(t, &cands(), now).expect("3 が選ばれる");
        assert_eq!(third, 3);
    }

    /// **前任者の停止が未確認なら引き渡さない**(同時編集で成果物が壊れる)。
    #[test]
    fn assign_refuses_handover_when_previous_not_stopped() {
        let mut c = Coordinator::new();
        let now = t0();
        let t = c.add_task("T", "d", &[], now);
        assert_eq!(c.try_assign(t, &cands(), now), Ok(1));

        // 停滞しただけ。プロセスはまだ生きているかもしれない。
        c.note_stalled(1, now);
        assert!(!c.task(t).unwrap().previous_stopped());

        let refusal = c.try_assign(t, &cands(), now).unwrap_err();
        assert_eq!(
            refusal,
            AssignRefusal::PreviousHolderNotStopped { previous: 1 }
        );
        assert_eq!(c.last_refusal(), Some(refusal));
        // 担当は変わっていない。
        assert_eq!(c.task(t).unwrap().assigned, Some(1));
        // 拒否の事実が履歴に残る。
        assert!(c
            .task(t)
            .unwrap()
            .history
            .iter()
            .any(|(_, e)| matches!(e, TaskEvent::HandoverRefused(_))));

        // 停止を提案 → 承認モードのゲートを通す。
        let p = c.propose_stop(t).expect("停止提案が出る");
        match &p {
            Proposal::StopSession { session, task, .. } => {
                assert_eq!(*session, 1);
                assert_eq!(*task, t);
            }
        }
        // 破壊的操作なので Ask / Agent では人の確認が要る。
        assert_eq!(
            gate_for(PermissionMode::Ask),
            ProposalGate::NeedsUserConfirm
        );
        assert_eq!(
            gate_for(PermissionMode::Agent),
            ProposalGate::NeedsUserConfirm
        );
        assert_eq!(gate_for(PermissionMode::Auto), ProposalGate::AutoApproved);

        // 実際に停止を確認して初めて引き渡せる。
        // 能力指定が無いので、作業中の 2 ではなく空いている 3 が選ばれる。
        assert!(c.confirm_stopped(t, now));
        assert_eq!(c.try_assign(t, &cands(), now), Ok(3));
    }

    /// プロセスが消えた場合は停止確認済みとして扱い、すぐ引き渡せる。
    #[test]
    fn dead_session_counts_as_confirmed_stopped() {
        let mut c = Coordinator::new();
        let now = t0();
        c.register_session(1);
        let t = c.add_task("T", "d", &[], now);
        assert_eq!(c.try_assign(t, &cands(), now), Ok(1));

        c.note_exited(1, now);
        assert!(c.task(t).unwrap().previous_stopped());
        assert_eq!(c.task(t).unwrap().state, TaskState::Failed);
        // 停止提案は不要。
        assert!(c.propose_stop(t).is_none());
        // 能力指定が無いので空いている 3 が選ばれる(2 は作業中)。
        assert_eq!(c.try_assign(t, &cands(), now), Ok(3));
    }

    /// 再試行の上限に達したら NeedsUser にして、それ以上回さない。
    #[test]
    fn max_attempts_exhaustion_yields_needs_user() {
        let mut c = Coordinator::new();
        let now = t0();
        let t = c.add_task("T", "d", &[], now);
        let list = vec![
            SessionInfo::new(1, SessionState::Idle, &[]),
            SessionInfo::new(2, SessionState::Idle, &[]),
            SessionInfo::new(3, SessionState::Idle, &[]),
            SessionInfo::new(4, SessionState::Idle, &[]),
        ];

        // 既定の上限は 3 回。
        assert_eq!(c.limits().max_attempts, DEFAULT_MAX_ATTEMPTS);
        for expected in 1..=DEFAULT_MAX_ATTEMPTS {
            let s = c
                .try_assign(t, &list, now)
                .expect("上限までは割り当てられる");
            assert_eq!(s, expected as u64);
            c.note_failed(t, s, "失敗", now);
            c.confirm_stopped(t, now);
        }
        assert_eq!(c.task(t).unwrap().attempts, DEFAULT_MAX_ATTEMPTS);

        // 4 回目は拒否され、タスクは人手待ちになる。
        let err = c.try_assign(t, &list, now).unwrap_err();
        assert_eq!(
            err,
            AssignRefusal::AttemptsExhausted {
                attempts: DEFAULT_MAX_ATTEMPTS
            }
        );
        assert_eq!(c.task(t).unwrap().state, TaskState::NeedsUser);

        // ユーザーへ上がっている。
        let esc = c.take_user_messages();
        assert_eq!(esc.len(), 1);
        assert_eq!(esc[0].kind, MsgKind::Escalation);

        // 何度呼んでも回り続けない(状態は NeedsUser のまま)。
        for _ in 0..5 {
            assert!(c.try_assign(t, &list, now).is_err());
        }
        assert_eq!(c.task(t).unwrap().state, TaskState::NeedsUser);
        assert_eq!(c.task(t).unwrap().attempts, DEFAULT_MAX_ATTEMPTS);
    }

    /// 候補がいなければ理由付きで断る(誰かに無理やり押し付けない)。
    #[test]
    fn assign_refuses_when_no_candidate() {
        let mut c = Coordinator::new();
        let now = t0();
        let t = c.add_task("T", "d", &[], now);
        assert_eq!(
            c.try_assign(t, &[], now),
            Err(AssignRefusal::NoEligibleCandidate)
        );
        assert_eq!(c.last_refusal(), Some(AssignRefusal::NoEligibleCandidate));
    }

    /// 再割り当ての理由と、引き継ぎ材料の持ち越しが記録される。
    #[test]
    fn redispatch_records_reason_and_carries_context() {
        let mut c = Coordinator::new();
        let now = t0();
        c.register_session(1);
        c.register_session(2);
        c.register_session(3);
        let t = c.add_task("移植", "パーサを移植する", &[], now);
        assert_eq!(c.try_assign(t, &cands(), now), Ok(1));

        // 前任が積み上げた成果。
        c.add_context(t, "lexer.rs は移植済み", now);
        c.add_context(t, "parser.rs は途中(式まで)", now);

        c.note_exited(1, now); // 落ちた → 停止確認済み
                               // 能力指定が無いので、空いている 3 が新担当になる(2 は作業中)。
        let (next, _) = c
            .redispatch(t, &cands(), ReassignReason::SessionDied, now)
            .expect("3 へ引き継ぐ");
        assert_eq!(next, 3);

        // 理由付きで履歴に残る。
        let task = c.task(t).unwrap();
        assert!(task.history.iter().any(|(_, e)| matches!(
            e,
            TaskEvent::Reassigned {
                reason: ReassignReason::SessionDied,
                to: 3,
                ..
            }
        )));

        // 引き継ぎ文が新担当の受信箱に積まれ、経過が入っている。
        let mb = c.mailbox(3).unwrap();
        assert_eq!(mb.len(), 1);
        let handoff = mb.iter().next().unwrap();
        assert_eq!(handoff.kind, MsgKind::Handoff);
        assert!(handoff.body.contains("lexer.rs は移植済み"));
        assert!(handoff.body.contains("parser.rs は途中"));

        // 待機中になれば配達される。
        let out = c.take_deliverable(&[(3, SessionState::Idle)]);
        assert_eq!(out.len(), 1);
        assert!(out[0].text.contains("引き継ぎ"));
    }

    /// 履歴とコンテキストは無限に伸びない。
    #[test]
    fn task_history_and_context_are_bounded() {
        let mut c = Coordinator::new();
        let now = t0();
        let t = c.add_task("T", "d", &[], now);
        for i in 0..(CONTEXT_CAP + HISTORY_CAP + 20) {
            c.add_context(t, format!("経過 {i}"), now);
        }
        let task = c.task(t).unwrap();
        assert_eq!(task.context().len(), CONTEXT_CAP);
        assert_eq!(task.history.len(), HISTORY_CAP);
        assert!(task.history_dropped() > 0);
    }

    /// 自分宛は捨てる。
    #[test]
    fn self_addressed_is_dropped() {
        let mut c = Coordinator::new();
        c.register_session(1);
        assert_eq!(
            c.enqueue(msg(1, 1, t0())),
            SendOutcome::Dropped {
                reason: DropReason::SelfAddressed
            }
        );
        assert_eq!(c.drop_count(DropKind::SelfAddressed), 1);
    }

    /// ユーザーへのエスカレーションはレート制限で塞がない。
    #[test]
    fn escalation_to_user_is_never_rate_limited() {
        let limits = Limits {
            global_limit: 1,
            pair_limit: 1,
            ..Limits::default()
        };
        let mut c = Coordinator::with_limits(limits);
        let now = t0();
        for i in 0..10 {
            c.escalate(format!("異常 {i}"), now);
        }
        // 受信箱の上限までは残る。捨てるとしても理由付き。
        assert!(!c.user_inbox().is_empty());
        assert_eq!(c.take_user_messages().len(), 10);
    }

    // ── ファイルの重なり (配る瞬間に止める) ──────────────────────────

    /// 状態と担当を自由に決めたタスクを 1 つ作る(判定のテーブルテスト用)。
    fn task_with(
        id: TaskId,
        title: &str,
        files: &[&str],
        state: TaskState,
        assigned: Option<SessionId>,
    ) -> Task {
        Task {
            id,
            title: title.to_string(),
            description: String::new(),
            assigned,
            state,
            attempts: 0,
            history: Vec::new(),
            required_caps: Vec::new(),
            files: normalize_files(files),
            failed_by: HashSet::new(),
            prev_holder_stopped: true,
            context: Vec::new(),
            history_dropped: 0,
        }
    }

    /// 何を「重なり」と呼ぶかをテーブルで固定する。
    ///
    /// glob の境界そのものは `lease::overlaps` 側のテストが持つ。ここで見るのは
    /// **どの状態のタスクがファイルを押さえているとみなすか**。
    #[test]
    fn 割り当て可否をテーブルで固定する() {
        // (相手のファイル, 相手の状態, 相手の担当, 候補のファイル, 渡す先, 重なるか)
        type Case = (
            &'static [&'static str],
            TaskState,
            Option<SessionId>,
            &'static [&'static str],
            SessionId,
            bool,
        );
        let cases: &[Case] = &[
            // 同じ具体パス → 止める
            (
                &["src/a.rs"],
                TaskState::Running,
                Some(1),
                &["src/a.rs"],
                2,
                true,
            ),
            // glob が具体パスを覆う / その逆 → どちらも止める
            (
                &["src/ui/**"],
                TaskState::Running,
                Some(1),
                &["src/ui/a.rs"],
                2,
                true,
            ),
            (
                &["src/ui/a.rs"],
                TaskState::Assigned,
                Some(1),
                &["src/ui/**"],
                2,
                true,
            ),
            // 停滞中でも「まだ生きて編集しているかもしれない」→ 止める
            (
                &["src/a.rs"],
                TaskState::Stalled,
                Some(1),
                &["src/a.rs"],
                2,
                true,
            ),
            // 区切りと ./ が違うだけ → 正規化して同じものとみなす
            (
                &["src\\ui\\a.rs"],
                TaskState::Running,
                Some(1),
                &["./src/ui/a.rs"],
                2,
                true,
            ),
            // 重ならない場所 → 通す
            (
                &["src/**"],
                TaskState::Running,
                Some(1),
                &["docs/x.md"],
                2,
                false,
            ),
            // `*` は `/` を越えない
            (
                &["src/*.rs"],
                TaskState::Running,
                Some(1),
                &["src/sub/a.rs"],
                2,
                false,
            ),
            // 同じセッションへ渡すなら重なってよい(1 人なら壊れない)
            (
                &["src/a.rs"],
                TaskState::Running,
                Some(1),
                &["src/a.rs"],
                1,
                false,
            ),
            // 終端(完了 / 人待ち)は手が離れている
            (
                &["src/a.rs"],
                TaskState::Done,
                Some(1),
                &["src/a.rs"],
                2,
                false,
            ),
            (
                &["src/a.rs"],
                TaskState::NeedsUser,
                Some(1),
                &["src/a.rs"],
                2,
                false,
            ),
            // 未割り当てはまだ誰も触っていない
            (
                &["src/a.rs"],
                TaskState::Pending,
                None,
                &["src/a.rs"],
                2,
                false,
            ),
            // 未申告(空)は判定できない — 止めようが無い
            (&[], TaskState::Running, Some(1), &["src/a.rs"], 2, false),
            (&["src/a.rs"], TaskState::Running, Some(1), &[], 2, false),
        ];

        for (i, (other, state, holder, mine, to, want)) in cases.iter().enumerate() {
            let tasks = vec![
                task_with(1, "先客", other, *state, *holder),
                task_with(2, "候補", mine, TaskState::Pending, None),
            ];
            let got = admit(&tasks, &tasks[1], *to);
            assert_eq!(
                !got.is_ok(),
                *want,
                "case {i}: {other:?}({state:?}/{holder:?}) vs {mine:?} → {to} : {got:?}"
            );
        }
    }

    /// 同じファイルを持つ 2 タスクを**別の**セッションへ配ろうとすると、
    /// 2 本目は割り当てられない(後勝ちにしない)。
    #[test]
    fn 同じファイルの2タスクを別セッションへ配ると2本目が拒否される() {
        let mut c = Coordinator::new();
        let now = t0();
        let a = c.add_task_with_files("認証を直す", "", &[], &["src/auth.rs"], now);
        let b = c.add_task_with_files("認証にテストを足す", "", &[], &["src/auth.rs"], now);

        let s1 = vec![SessionInfo::new(1, SessionState::Idle, &[])];
        let s2 = vec![SessionInfo::new(2, SessionState::Idle, &[])];
        assert_eq!(c.try_assign(a, &s1, now), Ok(1));

        let err = c.try_assign(b, &s2, now).unwrap_err();
        assert_eq!(
            err,
            AssignRefusal::FileOverlap {
                with: a,
                holder: Some(1)
            }
        );
        // 断ったのだから、担当は付いていない。
        assert_eq!(c.task(b).unwrap().assigned, None);
        assert_eq!(c.task(b).unwrap().state, TaskState::Pending);
        assert_eq!(c.last_refusal(), Some(err));
    }

    /// glob と具体パスの重なりも、配る前に止まる。
    #[test]
    fn globと具体パスの重なりも配る前に止まる() {
        let mut c = Coordinator::new();
        let now = t0();
        let a = c.add_task_with_files("UI を整理", "", &[], &["src/ui/**"], now);
        let b = c.add_task_with_files("ボタンを直す", "", &[], &["src/ui/a.rs"], now);

        assert_eq!(
            c.try_assign(a, &[SessionInfo::new(1, SessionState::Idle, &[])], now),
            Ok(1)
        );
        assert!(matches!(
            c.try_assign(b, &[SessionInfo::new(2, SessionState::Idle, &[])], now),
            Err(AssignRefusal::FileOverlap { .. })
        ));
    }

    /// **同じ**セッションへ渡すなら重なってよい。1 人が両方を持つのは安全で、
    /// ここまで断ると「衝突しない構成」まで潰してしまう。
    #[test]
    fn 同一セッションへの2タスクは重なっても通る() {
        let mut c = Coordinator::new();
        let now = t0();
        let a = c.add_task_with_files("A", "", &[], &["src/x.rs"], now);
        let b = c.add_task_with_files("B", "", &[], &["src/x.rs"], now);
        let one = vec![SessionInfo::new(1, SessionState::Idle, &[])];
        assert_eq!(c.try_assign(a, &one, now), Ok(1));
        assert_eq!(c.try_assign(b, &one, now), Ok(1));
    }

    /// 完了済み / 未割り当てのタスクとは重なってよい。
    #[test]
    fn 完了済みと未割り当てのタスクとは重なっても通る() {
        let now = t0();

        // (1) 完了済み
        let mut c = Coordinator::new();
        let a = c.add_task_with_files("済んだ仕事", "", &[], &["src/x.rs"], now);
        let b = c.add_task_with_files("続き", "", &[], &["src/x.rs"], now);
        assert_eq!(
            c.try_assign(a, &[SessionInfo::new(1, SessionState::Idle, &[])], now),
            Ok(1)
        );
        c.note_done(a, now);
        assert_eq!(
            c.try_assign(b, &[SessionInfo::new(2, SessionState::Idle, &[])], now),
            Ok(2)
        );

        // (2) 未割り当て(まだ誰も触っていない)
        let mut c = Coordinator::new();
        let _a = c.add_task_with_files("まだ配っていない", "", &[], &["src/y.rs"], now);
        let b = c.add_task_with_files("先に進める", "", &[], &["src/y.rs"], now);
        assert_eq!(
            c.try_assign(b, &[SessionInfo::new(2, SessionState::Idle, &[])], now),
            Ok(2)
        );
    }

    /// 断るなら、**次に何をすればよいか**まで出す。
    /// 相手の名前と重なったパターンが欠けたら、ユーザーは機能を切るだけ。
    #[test]
    fn 拒否の文面に相手の名前と重なったパターンが入る() {
        let mut c = Coordinator::new();
        let now = t0();
        let a = c.add_task_with_files("UI を整理", "", &[], &["src/ui/**"], now);
        let b = c.add_task_with_files(
            "ボタンを直す",
            "",
            &[],
            &["src/ui/a.rs", "docs/button.md"],
            now,
        );
        assert_eq!(
            c.try_assign(a, &[SessionInfo::new(1, SessionState::Idle, &[])], now),
            Ok(1)
        );

        let text = c.overlap_reason_for(b, 2).expect("重なるので文面が要る");
        // 誰と
        assert!(text.contains("UI を整理"), "相手の名前が無い: {text}");
        // どのパターンで
        assert!(text.contains("src/ui/**"), "相手のパターンが無い: {text}");
        assert!(text.contains("src/ui/a.rs"), "自分のパターンが無い: {text}");
        // どう分ければよいか(重ならない側は今すぐ渡せる)
        assert!(text.contains("docs/button.md"), "分割案が無い: {text}");
        // 誰が持っているか
        assert!(text.contains("session:1"), "保有者が無い: {text}");

        // 重ならないなら文面は要らない。
        assert!(c.overlap_reason_for(b, 1).is_none());
    }

    /// 断った事実と理由がタスクの履歴に残る(後から「なぜ止まったか」を追える)。
    #[test]
    fn 重なりでの拒否はタスクの履歴に残る() {
        let mut c = Coordinator::new();
        let now = t0();
        let a = c.add_task_with_files("先客", "", &[], &["src/x.rs"], now);
        let b = c.add_task_with_files("後から", "", &[], &["src/x.rs"], now);
        assert_eq!(
            c.try_assign(a, &[SessionInfo::new(1, SessionState::Idle, &[])], now),
            Ok(1)
        );
        assert!(c
            .try_assign(b, &[SessionInfo::new(2, SessionState::Idle, &[])], now)
            .is_err());

        let ev = c
            .task(b)
            .unwrap()
            .history
            .iter()
            .find_map(|(_, e)| match e {
                TaskEvent::OverlapRefused {
                    with,
                    holder,
                    pattern,
                    reason,
                } => Some((*with, *holder, pattern.clone(), reason.clone())),
                _ => None,
            })
            .expect("拒否が履歴に残っていない");
        assert_eq!(ev.0, a);
        assert_eq!(ev.1, Some(1));
        assert_eq!(ev.2, "src/x.rs");
        assert!(ev.3.contains("先客"), "履歴の文面が薄い: {}", ev.3);
    }

    /// 再割り当て(`redispatch`)も同じ判定を通る。
    /// **純粋関数を書いただけで呼ばれていない**ことが無いようにする番人。
    #[test]
    fn 再割り当ても重なりで止まる() {
        let mut c = Coordinator::new();
        let now = t0();
        let b = c.add_task_with_files("移植", "", &[], &["docs/**"], now);
        assert_eq!(
            c.try_assign(b, &[SessionInfo::new(2, SessionState::Idle, &[])], now),
            Ok(2)
        );
        // 別のタスクが src/parser.rs を押さえる。
        let a = c.add_task_with_files("パーサ修正", "", &[], &["src/parser.rs"], now);
        assert_eq!(
            c.try_assign(a, &[SessionInfo::new(1, SessionState::Idle, &[])], now),
            Ok(1)
        );
        // 進めるうちに、b も実は src/parser.rs を触ると分かった
        // (**次の割り当てから効く** — 既に配ったものは遡って止められない)。
        assert!(c.set_task_files(b, &["src/parser.rs"]));

        // 担当が停滞 → 停止を確認 → 別の空きへ回そうとする。
        c.note_stalled(2, now);
        assert!(c.confirm_stopped(b, now));
        let err = c
            .redispatch(
                b,
                &[SessionInfo::new(3, SessionState::Idle, &[])],
                ReassignReason::Stalled,
                now,
            )
            .unwrap_err();
        assert_eq!(
            err,
            AssignRefusal::FileOverlap {
                with: a,
                holder: Some(1)
            }
        );
        // 担当は前のまま。勝手に付け替えない。
        assert_eq!(c.task(b).unwrap().assigned, Some(2));
    }

    /// 警告だけでは足りない。**重ならない部分だけ先に渡す**案を出す。
    #[test]
    fn 重ならない部分だけ先に渡す分割案を返す() {
        let mut c = Coordinator::new();
        let now = t0();
        let _a = {
            let a = c.add_task_with_files("UI を整理", "", &[], &["src/ui/**"], now);
            assert_eq!(
                c.try_assign(a, &[SessionInfo::new(1, SessionState::Idle, &[])], now),
                Ok(1)
            );
            a
        };
        let b = c.add_task_with_files("ボタン", "", &[], &["src/ui/a.rs", "docs/guide.md"], now);

        let (now_ok, serial) = c.overlap_split_for(b, 2);
        assert_eq!(now_ok, vec!["docs/guide.md".to_string()]);
        assert_eq!(serial, vec!["src/ui/a.rs".to_string()]);

        // 同じセッションへ渡すなら分ける必要が無い。
        let (all, none) = c.overlap_split_for(b, 1);
        assert_eq!(
            all,
            vec!["src/ui/a.rs".to_string(), "docs/guide.md".to_string()]
        );
        assert!(none.is_empty());
    }

    /// 重なる相手が居ても、**同じセッションが候補に居るなら**そこへ渡せる。
    /// 「全員が重なる」ときだけ断る。
    #[test]
    fn 重なりを持つ候補だけを外して残りへ渡す() {
        let mut c = Coordinator::new();
        let now = t0();
        let a = c.add_task_with_files("A", "", &[], &["src/x.rs"], now);
        let b = c.add_task_with_files("B", "", &[], &["src/x.rs"], now);
        assert_eq!(
            c.try_assign(a, &[SessionInfo::new(1, SessionState::Idle, &[])], now),
            Ok(1)
        );
        // 2 は重なるので外れ、1 が残る(忙しくても 1 しか選べない)。
        let list = vec![
            SessionInfo::new(1, SessionState::Working, &[]),
            SessionInfo::new(2, SessionState::Idle, &[]),
        ];
        assert_eq!(c.try_assign(b, &list, now), Ok(1));
    }

    // ── 発信マーカーの解析 ───────────────────────────────────────────

    #[test]
    fn outbound_marker_parses_target_and_body() {
        let (to, body) = parse_outbound("[ZAI-TO:backend] テストを直してほしい").unwrap();
        assert_eq!(to, "backend");
        assert_eq!(body, "テストを直してほしい");
    }

    #[test]
    fn outbound_marker_accepts_all() {
        let (to, body) = parse_outbound("[ZAI-TO:ALL] 全員へ連絡").unwrap();
        assert_eq!(to, OUTBOUND_ALL);
        assert_eq!(body, "全員へ連絡");
    }

    /// 端末は行末を空白で埋めるので、そこだけは許す。
    #[test]
    fn outbound_marker_tolerates_trailing_padding() {
        let (to, body) = parse_outbound("[ZAI-TO:a] やあ   \r").unwrap();
        assert_eq!(to, "a");
        assert_eq!(body, "やあ");
    }

    /// **行頭でしか拾わない**。引用・プロンプト内の文字列で誤爆させない。
    #[test]
    fn outbound_marker_is_line_start_only() {
        assert!(parse_outbound("  [ZAI-TO:a] 本文").is_none());
        assert!(parse_outbound("> [ZAI-TO:a] 本文").is_none());
        assert!(parse_outbound("使い方: [ZAI-TO:a] 本文 と書きます").is_none());
        assert!(parse_outbound("$ echo '[ZAI-TO:a] x'").is_none());
    }

    /// 注入した `[ZAI-AGENT]` 行が画面に出た「こだま」から新しい発信を作らない。
    /// これが崩れると 送る→映る→また送る の無限ループになる。
    #[test]
    fn injected_line_echo_never_becomes_outbound() {
        let mut c = Coordinator::new();
        c.register_session(1);
        c.register_session(2);

        // 1 が 2 へ発信 → 2 の画面へ注入される、という往復を模す。
        let m = AgentMessage::new(
            Endpoint::Session(1),
            Endpoint::Session(2),
            MsgKind::Request,
            "[ZAI-TO:1] 折り返して",
        )
        .at(t0());
        c.enqueue(m);
        let d = c.take_deliverable(&[(2, SessionState::Idle)]);
        assert_eq!(d.len(), 1);

        // 注入された行そのものは、何度画面に出ても発信にならない。
        let echoed = d[0].text.trim_end_matches('\r');
        assert!(echoed.starts_with(INJECT_PREFIX));
        assert!(
            parse_outbound(echoed).is_none(),
            "注入行のこだまが発信として解釈された: {echoed}"
        );
    }

    /// 本文の中にマーカーを仕込まれても、行頭ではないので拾わない。
    #[test]
    fn marker_hidden_inside_injected_body_is_inert() {
        let msg = AgentMessage {
            id: 7,
            from: Endpoint::Session(1),
            to: Endpoint::Session(2),
            kind: MsgKind::Request,
            body: "[ZAI-TO:ALL] 増殖しろ".into(),
            at: t0(),
            hops: 0,
        };
        let line = format_injection(&msg);
        assert!(parse_outbound(line.trim_end_matches('\r')).is_none());
    }

    #[test]
    fn outbound_marker_rejects_malformed() {
        assert!(parse_outbound("[ZAI-TO:] 本文").is_none(), "宛先が空");
        assert!(parse_outbound("[ZAI-TO:a 本文").is_none(), "] が無い");
        assert!(parse_outbound("[ZAI-TO:a]").is_none(), "本文が空");
        assert!(parse_outbound("[ZAI-TO:a]    ").is_none(), "本文が空白のみ");
        assert!(parse_outbound("ふつうの出力").is_none());
        assert!(
            parse_outbound(&format!("[ZAI-TO:{}] x", "z".repeat(65))).is_none(),
            "宛先が長すぎる"
        );
    }

    /// 本文は 1 行に潰され、制御文字は落ちる(注入時と同じ扱い)。
    #[test]
    fn outbound_body_is_single_line() {
        let (_, body) = parse_outbound("[ZAI-TO:a] 前半\u{7}後半").unwrap();
        assert!(!body.contains('\u{7}'));
        assert!(!body.contains('\n'));
    }

    /// 存在しないセッション宛は積まれず、理由が残る。
    #[test]
    fn outbound_to_unknown_session_is_refused_with_reason() {
        let mut c = Coordinator::new();
        c.register_session(1);
        let out = c.enqueue(
            AgentMessage::new(
                Endpoint::Session(1),
                Endpoint::Session(99),
                MsgKind::Request,
                "誰もいない宛先",
            )
            .at(t0()),
        );
        assert_eq!(
            out,
            SendOutcome::Dropped {
                reason: DropReason::UnknownTarget
            }
        );
        assert_eq!(c.drop_count(DropKind::UnknownTarget), 1);
        let last = c.drop_log().last().expect("記録が残る");
        assert_eq!(last.reason, DropReason::UnknownTarget);
    }

    // ── クォータ監視 ────────────────────────────────────────────────

    fn st(epoch: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(epoch)
    }

    fn qsnap(agent: &str, account: &str, used: f32, at: SystemTime) -> quota::QuotaSnapshot {
        quota::QuotaSnapshot {
            agent: agent.into(),
            label: agent.into(),
            account: account.into(),
            used_fraction: Some(used),
            resets_at: None,
            window: None,
            plan: None,
            observed_events: Vec::new(),
            source: quota::SourceKind::Vendor,
            measured_at: Some(at),
        }
    }

    /// 取り込みを重ねると燃焼速度が貯まり、枯渇予測が出る。
    #[test]
    fn quota_watch_projects_after_two_samples() {
        let mut w = QuotaWatch::new();
        w.set_running(vec![("codex".to_string(), 2)]);
        w.apply(vec![qsnap("codex", "openai", 0.10, st(0))], st(0));
        // 1 点だけでは材料不足 (推測を出さない)
        let acc = w.accounts(st(0));
        assert_eq!(acc.len(), 1);
        assert_eq!(acc[0].projection, quota::Projection::InsufficientData);
        assert_eq!(acc[0].running_agents, 2);

        w.apply(vec![qsnap("codex", "openai", 0.40, st(600))], st(600));
        let acc = w.accounts(st(600));
        // 0.3/600s = 0.0005/s、残り 0.6 → 1200 秒
        assert_eq!(
            acc[0].projection,
            quota::Projection::Exhaustion(Duration::from_secs(1200))
        );
        assert_eq!(acc[0].confidence, quota::Confidence::Measured);
        assert_eq!(w.applied(), 2);
    }

    /// 観測した上限イベントはスナップショットへ合流し、助言は Stop になる。
    #[test]
    fn quota_watch_events_drive_stop_advice() {
        let mut w = QuotaWatch::new();
        w.apply(vec![qsnap("codex", "openai", 0.10, st(0))], st(0));
        w.note_rate_limited("codex", "usage limit reached", st(100));
        assert_eq!(w.events().len(), 1);
        assert!(!w.snapshots()[0].observed_events.is_empty(), "合流している");
        let advice = w.advice(st(200));
        assert_eq!(advice.len(), 1);
        assert_eq!(advice[0].0, "openai");
        assert_eq!(w.worst_advice(st(200)).severity(), 2, "Stop");
        // 余裕がある間は黙る
        let mut calm = QuotaWatch::new();
        calm.apply(vec![qsnap("codex", "openai", 0.05, st(0))], st(0));
        assert_eq!(calm.worst_advice(st(0)), quota::Advice::Ok);
    }

    /// 端末出力からの検知は terminal.rs の判定をそのまま使う。
    #[test]
    fn quota_watch_note_output_uses_shared_detector() {
        let mut w = QuotaWatch::new();
        assert!(w.note_output("claude", "5-hour limit reached ∙ resets 3am\n", st(1)));
        assert!(!w.note_output("claude", "ふつうの出力\n", st(2)));
        assert_eq!(w.events().len(), 1);
    }

    /// イベントは上限付きで、古いものから捨てる。
    #[test]
    fn quota_watch_events_are_bounded() {
        let mut w = QuotaWatch::new();
        for i in 0..(QUOTA_EVENT_CAP as u64 + 20) {
            w.note_rate_limited("codex", "usage limit reached", st(i));
        }
        assert_eq!(w.events().len(), QUOTA_EVENT_CAP);
        assert_eq!(w.events()[0].at, st(20), "古いものから捨てる");
    }

    /// 背景スレッド + チャネルで読み取れる (UI スレッドは触らない)。
    /// TTL 内の再呼び出しでは新しいスキャンを始めない。
    #[test]
    fn quota_watch_background_refresh_lands() {
        let mut w = QuotaWatch::new();
        w.force_refresh();
        let deadline = Instant::now() + Duration::from_secs(10);
        while w.applied() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            w.refresh_if_stale(); // TTL 内なので取り込みだけ行う
        }
        assert_eq!(w.applied(), 1, "背景スキャンが 1 回だけ取り込まれる");
        assert_eq!(
            w.snapshots().len(),
            quota::AGENT_QUOTAS.len(),
            "記述子の数だけ行が出る"
        );
    }

    /// 単価表 (テスト用。1 モデルだけ持つ)。
    struct FlatPrice(f64);

    impl quota::PriceLookup for FlatPrice {
        fn rate(&self, _model: &str) -> Option<quota::ModelRate> {
            Some(quota::ModelRate {
                input: self.0,
                output: self.0,
                cache_write: self.0,
                cache_read: self.0,
            })
        }
        fn currency(&self) -> &str {
            "$"
        }
    }

    fn agent_tokens(label: &str, output: u64) -> quota::AgentTokens {
        let usage = quota::TokenUsage {
            output,
            ..Default::default()
        };
        quota::AgentTokens {
            agent: label.into(),
            label: label.into(),
            account: label.into(),
            total: usage,
            by_model: vec![("m".into(), usage)],
            turns: 1,
            truncated: false,
        }
    }

    /// 窓 / 今日 / セッションは別々に数えられ、どれも 0 なら 0 を返す。
    #[test]
    fn 窓と今日とセッションの推定コストを別々に取れる() {
        let mut w = QuotaWatch::new();
        // 1 単位 = 100 万トークンあたり 1.0 → 100 万トークンで $1.00
        let prices = FlatPrice(1.0);
        // 何も読めていないうちは 0 (「不明」を 0 と言い張るのではなく、
        // 判定側が上限未設定なら見に来ない)
        assert_eq!(w.cost_today(&prices), 0.0);
        assert_eq!(w.cost_session(&prices), 0.0);
        assert_eq!(w.cost_total(&prices), None);
        w.set_token_scan(TokenScan {
            window: vec![agent_tokens("a", 3_000_000)],
            today: vec![agent_tokens("a", 2_000_000)],
            session: vec![agent_tokens("a", 1_000_000)],
        });
        assert_eq!(w.cost_total(&prices).unwrap().amount, 3.0);
        assert_eq!(w.cost_today(&prices), 2.0);
        assert_eq!(w.cost_session(&prices), 1.0);
        // 表示用の窓は従来どおり `tokens()` から取れる
        assert_eq!(w.tokens().len(), 1);
        assert_eq!(w.token_scan().today.len(), 1);
    }

    /// セッションの起点は注入できる (テストが実時間に依存しない)。
    #[test]
    fn セッションの起点は注入できる() {
        let mut w = QuotaWatch::new();
        let t = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000);
        w.set_session_since(t);
        // 起点を変えただけでは何も読まない (副作用が無い)
        assert_eq!(w.applied(), 0);
        assert!(w.token_scan().session.is_empty());
    }

    /// 日 (UTC) をまたいだら日次だけ 0 に戻り、セッションぶんは残る。
    #[test]
    fn 日をまたいだら今日ぶんだけリセットされる() {
        let mut w = QuotaWatch::new();
        w.set_token_scan(TokenScan {
            window: vec![agent_tokens("a", 3)],
            today: vec![agent_tokens("a", 2)],
            session: vec![agent_tokens("a", 1)],
        });
        let day0 = std::time::UNIX_EPOCH + Duration::from_secs(quota::DAY_SECS * 100);
        // 同じ日のうちは何もしない (同じ日に何度呼んでも副作用ゼロ)
        assert!(w.roll_day_if_needed(day0));
        assert!(w.token_scan().today.is_empty(), "跨いだので今日ぶんは空");
        assert_eq!(w.token_scan().session.len(), 1, "セッションぶんは残る");
        assert_eq!(w.token_scan().window.len(), 1, "窓ぶんは残る");
        w.set_token_scan(TokenScan {
            today: vec![agent_tokens("a", 2)],
            ..w.token_scan().clone()
        });
        for _ in 0..3 {
            assert!(
                !w.roll_day_if_needed(day0 + Duration::from_secs(quota::DAY_SECS - 1)),
                "同じ日 (UTC) では跨いだことにしない"
            );
        }
        assert_eq!(w.token_scan().today.len(), 1, "同じ日なら捨てない");
        // 24 時間ちょうどで次の日
        assert!(w.roll_day_if_needed(day0 + Duration::from_secs(quota::DAY_SECS)));
        assert!(w.token_scan().today.is_empty());
    }

    // ── 鎖① をエンドツーエンドで測るための入口 (tools/coordinator-bench.sh) ──

    /// 配る前の分割 (鎖①) を**プロセスの外から**回すための入口。
    /// **通常のテスト実行では環境変数が無いので、何もせずに返る。**
    ///
    /// `coordinator` は GUI からしか到達せず、テストバイナリには `zai` の CLI
    /// 入口が無い。そこで [`crate::union`] の `merge_driver_helper` と同じ形で
    /// 自分自身を呼ぶ。**位置引数は libtest がテスト名の絞り込みとして食う**ので、
    /// 受け渡しは全て環境変数で行う。
    ///
    /// * `ZV_COORD_TASKS` … 担当表のパス。1 行 = `<ラベル>\t<パターン>[,…]`
    /// * `ZV_COORD_MODE`  … `naive` (分割を通さない) / `coord` (通す)
    /// * `ZV_COORD_AGENTS`… セッション数 (既定 = タスク数)
    /// * `ZV_COORD_VERBOSE` … 立てると [`overlap_reason`] の文面も `# ` 付きで出す
    ///
    /// 出力は 1 行 1 タスクの
    /// `task\t<ラベル>\t<セッション>\t<ok|split|refused>\t<配ったパターン…>`
    /// と、集計 1 行。**配ったものをそのまま出す**ので、ハーネス側が
    /// 「本当に互いに素なものを配ったか」を独立に検査できる (§3.11.5 の教訓)。
    ///
    /// `naive` は「同じプロセス起動・同じ解析・同じ出力で、判定だけしない」
    /// 空回しでもあるので、**ハーネスの費用はこの段との差で引ける**。
    #[test]
    fn assign_helper() {
        let Ok(path) = std::env::var("ZV_COORD_TASKS") else {
            return;
        };
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let mode = std::env::var("ZV_COORD_MODE").unwrap_or_else(|_| "coord".to_string());
        let verbose = std::env::var("ZV_COORD_VERBOSE").is_ok();

        let mut specs: Vec<(String, Vec<String>)> = Vec::new();
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            let (id, rest) = line.split_once('\t').unwrap_or((line, ""));
            let pats: Vec<String> = rest
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
            if pats.is_empty() {
                continue;
            }
            specs.push((id.to_string(), pats));
        }
        let agents: u64 = std::env::var("ZV_COORD_AGENTS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or_else(|| specs.len().max(1) as u64);

        let mut out = String::new();
        let (mut full, mut split_n, mut refused) = (0u32, 0u32, 0u32);
        let (mut granted, mut dropped) = (0u32, 0u32);

        if mode == "naive" {
            for (i, (id, pats)) in specs.iter().enumerate() {
                let s = (i as u64 % agents) + 1;
                full += 1;
                granted += pats.len() as u32;
                out.push_str(&format!("task\t{id}\t{s}\tok\t{}\n", pats.join(" ")));
            }
        } else {
            let mut c = Coordinator::new();
            let now = Instant::now();
            let ids: Vec<TaskId> = specs
                .iter()
                .map(|(id, pats)| {
                    let refs: Vec<&str> = pats.iter().map(|s| s.as_str()).collect();
                    c.add_task_with_files(id, "", &[], &refs, now)
                })
                .collect();
            for (i, tid) in ids.iter().enumerate() {
                let s = (i as u64 % agents) + 1;
                let want = specs[i].1.len() as u32;
                let cand = vec![SessionInfo::new(s, SessionState::Idle, &[])];
                if c.try_assign(*tid, &cand, now).is_ok() {
                    full += 1;
                    granted += want;
                    out.push_str(&format!(
                        "task\t{}\t{s}\tok\t{}\n",
                        specs[i].0,
                        specs[i].1.join(" ")
                    ));
                    continue;
                }
                // 断る前に「重ならない部分だけ」を出す。**文面 (overlap_reason)
                // も必ず通す** — 出荷経路 (GUI) と同じ順序で呼ばないと、
                // 測っているものが違ってしまう。
                let reason = c.overlap_reason_for(*tid, s);
                let (now_ok, serial) = c.overlap_split_for(*tid, s);
                if !serial.is_empty() {
                    out.push_str(&format!("serial\t{}\t{}\n", specs[i].0, serial.join(" ")));
                }
                if verbose {
                    for l in reason.iter().flat_map(|r| r.lines()) {
                        out.push_str(&format!("# {l}\n"));
                    }
                }
                let ok = if now_ok.is_empty() {
                    false
                } else {
                    let refs: Vec<&str> = now_ok.iter().map(|s| s.as_str()).collect();
                    c.set_task_files(*tid, &refs);
                    c.try_assign(*tid, &cand, now).is_ok()
                };
                if ok {
                    split_n += 1;
                    granted += now_ok.len() as u32;
                    dropped += want.saturating_sub(now_ok.len() as u32);
                    out.push_str(&format!(
                        "task\t{}\t{s}\tsplit\t{}\n",
                        specs[i].0,
                        now_ok.join(" ")
                    ));
                } else {
                    refused += 1;
                    dropped += want;
                    out.push_str(&format!("task\t{}\t{s}\trefused\t\n", specs[i].0));
                }
            }
        }
        out.push_str(&format!(
            "summary\tfull={full}\tsplit={split_n}\trefused={refused}\tgranted={granted}\tdropped={dropped}\n"
        ));
        // **`print!` を使わない。** libtest は `print!` 系だけを横取りするので、
        // `process::exit` すると捕まえられた出力ごと消える。fd 1 へ直接書く。
        use std::io::Write;
        let mut so = std::io::stdout().lock();
        let _ = so.write_all(out.as_bytes());
        let _ = so.flush();
        std::process::exit(0);
    }

    /// **python3 を呼ぶハーネスは UTF-8 を明示していること。**
    ///
    /// Windows (Git Bash / PowerShell) の既定コードページは UTF-8 ではないので、
    /// Python が日本語を stdout へ書いた瞬間に
    /// `UnicodeEncodeError: 'charmap' codec can't encode characters` で落ちる。
    /// **実際に CI の `probe (windows-latest)` がこれで赤くなり、
    /// 同じ穴が他に 10 本あった** — 落ちたのは、たまたま CI が Windows で
    /// 回していた 1 本だけだったから。
    ///
    /// macOS / Linux では既定が UTF-8 なので、**手元では永久に気付けない**。
    #[test]
    fn python3を呼ぶハーネスはutf8を明示している() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tools");
        let mut missing: Vec<String> = Vec::new();
        for e in std::fs::read_dir(&dir).expect("tools/").flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("sh") {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&p) else {
                continue;
            };
            let src = src.replace("\r\n", "\n");
            if !src.contains("python3") {
                continue;
            }
            // どちらか一方でも宣言してあれば、Windows でも UTF-8 で書ける。
            if src.contains("PYTHONUTF8") || src.contains("PYTHONIOENCODING") {
                continue;
            }
            missing.push(
                p.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string(),
            );
        }
        missing.sort();
        assert!(
            missing.is_empty(),
            "python3 を呼ぶのに UTF-8 を明示していないハーネス: {missing:?}\n\
             `export PYTHONUTF8=\"${{PYTHONUTF8:-1}}\"` と\n\
             `export PYTHONIOENCODING=\"${{PYTHONIOENCODING:-utf-8}}\"` を足すこと。\n\
             Windows の既定コードページでは日本語を書いた瞬間に落ちる。"
        );
    }

    /// ハーネスが `--exact` に渡す名前が、実際のモジュール位置とずれていないこと。
    /// **ずれるとハーネスは「0 件のテストが走った」で静かに緑になる。**
    #[test]
    fn assign_helper_name_matches_harness() {
        let m = module_path!();
        let rel = m.split_once("::").map(|(_, r)| r).unwrap_or(m);
        let want = format!("{rel}::assign_helper");
        let sh = include_str!("../tools/coordinator-bench.sh").replace("\r\n", "\n");
        assert!(
            sh.contains(&want),
            "tools/coordinator-bench.sh が {want} を指していません"
        );
    }
}
